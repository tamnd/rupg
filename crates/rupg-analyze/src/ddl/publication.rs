//! `CREATE PUBLICATION`, `ALTER PUBLICATION` and the other statements on a publication, as `CreatePublication`, `AlterPublication`, `AlterPublicationOwner_internal` and `parse_publication_options` of `publicationcmds.c` run them, with the checks of `publication_add_relation`, `check_publication_add_relation`, `pub_collist_validate`, `publication_add_schema` and `check_publication_add_schema` of `pg_publication.c`.

use rupg_catalog::{
    BOOTSTRAP_SUPERUSER, FIRST_NORMAL_OID, ObjRef, PG_PUBLICATION, PG_PUBLICATION_NAMESPACE,
    PG_PUBLICATION_REL, PG_TOAST_NAMESPACE, Publication, PublicationRel,
};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    AlterOwnerStmt, AlterPublicationAction, AlterPublicationStmt, CreatePublicationStmt, DefElem,
    List, Node, PublicationObjSpecType, PublicationTable, RenameStmt,
};

use super::comment::not_owner;
use super::objaddr::found_oid;
use super::reloptions::def_boolean;
use super::sequence::conflicting;
use super::tsearch::def_string;
use super::{Definer, Found, not_yet, references, relkind_detail};
use crate::PG_CATALOG_NAMESPACE;
use crate::agg::Kind;
use crate::expr::{Expr, ExprKind, FuncForm};

/// The values of the options of `parse_publication_options`, or `None` for an option that the statement does not give.
#[derive(Default)]
struct Options {
    /// `publish`: `pubinsert`, `pubupdate`, `pubdelete` and `pubtruncate`.
    publish: Option<[bool; 4]>,
    /// `publish_via_partition_root`.
    via_root: Option<bool>,
    /// `publish_generated_columns`.
    gencols: Option<u8>,
}

impl Options {
    /// True when the statement gives an option.
    fn given(&self) -> bool {
        self.publish.is_some() || self.via_root.is_some() || self.gencols.is_some()
    }
}

/// `parse_publication_options`.
fn publication_options(options: &List) -> Result<Options> {
    let mut out = Options::default();
    for node in options.iter().flatten() {
        let Node::DefElem(def) = node else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        match name {
            "publish" => {
                if out.publish.is_some() {
                    return Err(conflicting(def));
                }
                let text = def_string(def)?;
                let Some(items) = rupg_types::split_identifier_string(&text, b',') else {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "invalid list syntax in parameter \"publish\"",
                    ));
                };
                let mut actions = [false; 4];
                for item in items {
                    let at = match &*item {
                        "insert" => 0,
                        "update" => 1,
                        "delete" => 2,
                        "truncate" => 3,
                        _ => {
                            return Err(Error::new(
                                SqlState::SYNTAX_ERROR,
                                format!(
                                    "unrecognized value for publication option \"publish\": \"{item}\""
                                ),
                            ));
                        }
                    };
                    actions[at] = true;
                }
                out.publish = Some(actions);
            }
            "publish_via_partition_root" => {
                if out.via_root.is_some() {
                    return Err(conflicting(def));
                }
                out.via_root = Some(def_boolean(name, def.arg.as_ref())?);
            }
            "publish_generated_columns" => {
                if out.gencols.is_some() {
                    return Err(conflicting(def));
                }
                out.gencols = Some(gencols_option(def)?);
            }
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("unrecognized publication parameter: \"{name}\""),
                ));
            }
        }
    }
    Ok(out)
}

/// `defGetGeneratedColsOption`: `n` for `none` and `s` for `stored`, in any case.
fn gencols_option(def: &DefElem) -> Result<u8> {
    let text = match def.arg {
        Some(_) => def_string(def)?,
        None => String::new(),
    };
    if def.arg.is_some() {
        if text.eq_ignore_ascii_case("none") {
            return Ok(b'n');
        }
        if text.eq_ignore_ascii_case("stored") {
            return Ok(b's');
        }
    }
    Err(Error::new(
        SqlState::SYNTAX_ERROR,
        format!(
            "invalid value for publication parameter \"{}\": \"{text}\"",
            def.defname.as_deref().unwrap_or_default()
        ),
    )
    .with_detail("Valid values are \"none\" and \"stored\"."))
}

/// The objects of `ObjectsInPublicationToOids`: the tables, the tables of the `EXCEPT` clause and the schemas.
#[derive(Default)]
struct Objects<'s> {
    tables: Vec<&'s PublicationTable>,
    except: Vec<&'s PublicationTable>,
    schemas: Vec<u32>,
}

/// A table of the statement, as `PublicationRelInfo`.
struct PubTable<'s> {
    found: Found,
    name: String,
    /// The `WHERE` clause as the statement gives it.
    clause: Option<&'s Node>,
    /// The row filter after `TransformPubWhereClauses`.
    qual: Option<Expr>,
    columns: Vec<&'s str>,
    except: bool,
}

/// The message of `check_publication_add_relation` for a table that cannot go in the publication.
fn cannot_add(table: &PubTable<'_>, qualified: &str, detail: &str) -> Error {
    let message = if table.except {
        format!("cannot specify relation \"{qualified}\" in the publication EXCEPT clause")
    } else {
        format!("cannot add relation \"{}\" to publication", table.name)
    };
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message).with_detail(detail)
}

/// The words of `get_relkind_objtype` for the error of `aclcheck_error` with `ACLCHECK_NOT_OWNER`.
fn owner_word(kind: u8) -> &'static str {
    match kind {
        b'S' => "sequence",
        b'v' => "view",
        b'm' => "materialized view",
        b'f' => "foreign table",
        b'i' | b'I' => "index",
        b'c' => "type",
        _ => "table",
    }
}

/// The error of `GetPublicationByName` for a publication that does not exist.
fn no_publication(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
}

/// The error of `check_simple_rowfilter_expr_walker`.
fn invalid_filter(expr: &Expr, detail: &str) -> Error {
    let error = Error::new(SqlState::FEATURE_NOT_SUPPORTED, "invalid publication WHERE expression")
        .with_detail(detail);
    match expr.place() {
        Some(at) => error.at(at),
        None => error,
    }
}

/// `contain_mutable_or_user_functions_checker`: true for a function that is not immutable or that a user made.
fn mutable_or_user(oid: u32) -> bool {
    if oid >= FIRST_NORMAL_OID {
        return true;
    }
    rupg_pgcatalog::builtin::proc_by_oid(oid).is_none_or(|p| p.volatile != b'i')
}

/// `check_simple_rowfilter_expr_walker`: a row filter can only have columns, constants, built-in operators, built-in types, built-in collations and immutable built-in functions.
fn check_simple_filter(expr: &Expr) -> Result<()> {
    let mut error = None;
    expr.find(0, &mut |e, _| {
        let user_op = "User-defined operators are not allowed.";
        let mut detail = match &e.kind {
            ExprKind::Var(var) if var.attnum < 0 => Some("System columns are not allowed."),
            ExprKind::Var(_) => None,
            ExprKind::Func(f) => match f.form {
                FuncForm::Operator(op) if op >= FIRST_NORMAL_OID => Some(user_op),
                _ => None,
            },
            ExprKind::NullIf { equal: func, .. }
            | ExprKind::Distinct { equal: func, .. }
            | ExprKind::ScalarArrayOp { func, .. }
                if *func >= FIRST_NORMAL_OID =>
            {
                Some(user_op)
            }
            ExprKind::RowCompare(rc)
                if rc.ops.iter().any(|op| {
                    matches!(&op.kind, ExprKind::Func(f) if matches!(f.form, FuncForm::Operator(o) if o >= FIRST_NORMAL_OID))
                }) =>
            {
                Some(user_op)
            }
            ExprKind::NullIf { .. }
            | ExprKind::Distinct { .. }
            | ExprKind::ScalarArrayOp { .. }
            | ExprKind::RowCompare(_)
            | ExprKind::Const(_)
            | ExprKind::Bool(..)
            | ExprKind::Relabel(..)
            | ExprKind::Collate(..)
            | ExprKind::Case(_)
            | ExprKind::CaseTest
            | ExprKind::Array { .. }
            | ExprKind::Row(..)
            | ExprKind::Coalesce(_)
            | ExprKind::MinMax { .. }
            | ExprKind::Xml(_)
            | ExprKind::NullTest(..)
            | ExprKind::BooleanTest(..) => None,
            _ => Some(
                "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.",
            ),
        };
        if detail.is_none() {
            let funcs: Vec<u32> = match &e.kind {
                ExprKind::Func(f) => vec![f.oid],
                ExprKind::NullIf { equal, .. } | ExprKind::Distinct { equal, .. } => vec![*equal],
                ExprKind::ScalarArrayOp { func, .. } => vec![*func],
                _ => Vec::new(),
            };
            if e.ty >= FIRST_NORMAL_OID {
                detail = Some("User-defined types are not allowed.");
            } else if funcs.into_iter().any(mutable_or_user) {
                detail = Some("User-defined or built-in mutable functions are not allowed.");
            } else if matches!(e.kind, ExprKind::Collate(_, c) if c >= FIRST_NORMAL_OID) {
                detail = Some("User-defined collations are not allowed.");
            }
        }
        let detail = detail?;
        error = Some(invalid_filter(e, detail));
        Some(())
    });
    error.map_or(Ok(()), Err)
}

impl<'s> Definer<'_, '_> {
    /// The publication with the name, which must exist.
    fn publication_named(&self, name: &str) -> Result<Publication> {
        self.catalog.publication_by_name(name).cloned().ok_or_else(|| no_publication(name))
    }

    /// `ObjectsInPublicationToOids`.
    fn publication_objects(&self, objects: &'s List) -> Result<Objects<'s>> {
        let mut out = Objects::default();
        for node in objects.iter().flatten() {
            let Node::PublicationObjSpec(spec) = node else { continue };
            match spec.pubobjtype {
                PublicationObjSpecType::PUBLICATIONOBJ_EXCEPT_TABLE => {
                    out.except.extend(spec.pubtable.as_deref());
                }
                PublicationObjSpecType::PUBLICATIONOBJ_TABLE => {
                    out.tables.extend(spec.pubtable.as_deref());
                }
                PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_SCHEMA => {
                    let name = spec.name.as_deref().unwrap_or_default();
                    let schema = self.namespace_oid(name).ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_SCHEMA,
                            format!("schema \"{name}\" does not exist"),
                        )
                    })?;
                    if !out.schemas.contains(&schema) {
                        out.schemas.push(schema);
                    }
                }
                PublicationObjSpecType::PUBLICATIONOBJ_TABLES_IN_CUR_SCHEMA => {
                    let Some(&schema) = self.an.env.search_path().first() else {
                        return Err(Error::new(
                            SqlState::UNDEFINED_SCHEMA,
                            "no schema has been selected for CURRENT_SCHEMA",
                        ));
                    };
                    if !out.schemas.contains(&schema) {
                        out.schemas.push(schema);
                    }
                }
                other => {
                    return Err(Error::internal(format!(
                        "invalid publication object type {}",
                        other.0
                    )));
                }
            }
        }
        Ok(out)
    }

    /// The schema of a relation that a lookup found.
    fn found_namespace(&self, found: Found) -> u32 {
        match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.namespace),
            Found::Builtin(row) => row.namespace,
        }
    }

    /// `OpenTableList`: the tables of the list, without the tables that come again, and with the children of each table that the statement does not name with `ONLY`.
    fn open_tables(&self, tables: &[&'s PublicationTable]) -> Result<Vec<PubTable<'s>>> {
        let mut out: Vec<PubTable<'s>> = Vec::new();
        let mut with_filter: Vec<u32> = Vec::new();
        let mut with_columns: Vec<u32> = Vec::new();
        for table in tables {
            let Some(rv) = table.relation.as_deref() else { continue };
            let found = self.lookup_relation(rv)?;
            let name = self.found_name(found);
            let columns: Vec<&'s str> = table
                .columns
                .iter()
                .flatten()
                .filter_map(|c| match c {
                    Node::String(s) => Some(&**s),
                    _ => None,
                })
                .collect();
            let has_filter = table.whereClause.is_some();
            let has_columns = !columns.is_empty();
            if out.iter().any(|t| found_oid(t.found) == found_oid(found)) {
                if has_filter || with_filter.contains(&found_oid(found)) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("conflicting or redundant WHERE clauses for table \"{name}\""),
                    ));
                }
                if has_columns || with_columns.contains(&found_oid(found)) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("conflicting or redundant column lists for table \"{name}\""),
                    ));
                }
                continue;
            }
            let entry = |found: Found, name: String| PubTable {
                found,
                name,
                clause: table.whereClause.as_ref(),
                qual: None,
                columns: columns.clone(),
                except: table.except,
            };
            out.push(entry(found, name.clone()));
            if has_filter {
                with_filter.push(found_oid(found));
            }
            if has_columns {
                with_columns.push(found_oid(found));
            }
            let Found::User(oid) = found else { continue };
            if !rv.inh || self.kind_of(found) == b'p' {
                continue;
            }
            for child in self.catalog.all_inheritors(oid) {
                let child = Found::User(child);
                if out.iter().any(|t| found_oid(t.found) == found_oid(child)) {
                    if has_filter || with_filter.contains(&found_oid(child)) {
                        return Err(Error::new(
                            SqlState::DUPLICATE_OBJECT,
                            format!("conflicting or redundant WHERE clauses for table \"{name}\""),
                        ));
                    }
                    if has_columns || with_columns.contains(&found_oid(child)) {
                        return Err(Error::new(
                            SqlState::DUPLICATE_OBJECT,
                            format!("conflicting or redundant column lists for table \"{name}\""),
                        ));
                    }
                    continue;
                }
                out.push(entry(child, self.found_name(child)));
                if has_filter {
                    with_filter.push(found_oid(child));
                }
                if has_columns {
                    with_columns.push(found_oid(child));
                }
            }
        }
        Ok(out)
    }

    /// `TransformPubWhereClauses`: the row filter of each table, which reads only the table, must be a boolean and a simple expression.
    fn transform_filters(&mut self, tables: &mut [PubTable<'s>], via_root: bool) -> Result<()> {
        for table in tables.iter_mut() {
            let Some(node) = table.clause else { continue };
            if !via_root && self.kind_of(table.found) == b'p' {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("cannot use publication WHERE clause for relation \"{}\"", table.name),
                )
                .with_detail(
                    "WHERE clause cannot be used for a partitioned table when publish_via_partition_root is false.",
                ));
            }
            let Found::User(oid) = table.found else { continue };
            self.an.scope = crate::from::Scope::default();
            if let Some(rel) = self.catalog.relation(oid) {
                let columns: Vec<(String, u32, i32)> =
                    rel.columns.iter().map(crate::from::scope_column).collect();
                let relname = rel.name.clone();
                let row_type = rel.row_type;
                self.an.add_table(oid, &relname, &columns, row_type);
            }
            let expr = self.an.with_kind(Kind::Where, |an| an.transform(Some(node)))?;
            let mut expr = self.an.coerce_to_boolean(expr, "PUBLICATION WHERE")?;
            crate::virtual_columns::expand_virtual_expr(&mut expr, oid, self.catalog)?;
            check_simple_filter(&expr)?;
            table.qual = Some(expr);
        }
        Ok(())
    }

    /// `CheckPubRelationColumnList`.
    fn check_column_lists(
        &self,
        publication: &str,
        tables: &[PubTable<'s>],
        publish_schema: bool,
        via_root: bool,
    ) -> Result<()> {
        for table in tables.iter().filter(|t| !t.columns.is_empty()) {
            let error = || {
                Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "cannot use column list for relation \"{}.{}\" in publication \"{publication}\"",
                        self.schema_name(self.found_namespace(table.found)),
                        table.name
                    ),
                )
            };
            if publish_schema {
                return Err(error().with_detail(
                    "Column lists cannot be specified in publications containing FOR TABLES IN SCHEMA elements.",
                ));
            }
            if !via_root && self.kind_of(table.found) == b'p' {
                return Err(error().with_detail(
                    "Column lists cannot be specified for partitioned tables when publish_via_partition_root is false.",
                ));
            }
        }
        Ok(())
    }

    /// `pub_collist_validate`: the numbers of the columns of the list in order.
    fn publication_columns(&self, table: &PubTable<'s>) -> Result<Vec<i16>> {
        let Found::User(oid) = table.found else { return Ok(Vec::new()) };
        let Some(rel) = self.catalog.relation(oid) else { return Ok(Vec::new()) };
        let mut numbers: Vec<i16> = Vec::new();
        for &name in &table.columns {
            let number = rel.columns.iter().position(|c| !c.dropped && c.name == name);
            let Some(number) = number else {
                if super::table::SYSTEM_COLUMNS.contains(&name) {
                    return Err(Error::new(
                        SqlState::INVALID_COLUMN_REFERENCE,
                        format!("cannot use system column \"{name}\" in publication column list"),
                    ));
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("column \"{name}\" of relation \"{}\" does not exist", table.name),
                ));
            };
            if rel.columns[number].generated == b'v' {
                return Err(Error::new(
                    SqlState::INVALID_COLUMN_REFERENCE,
                    format!(
                        "cannot use virtual generated column \"{name}\" in publication column list"
                    ),
                ));
            }
            let number = i16::try_from(number + 1).unwrap_or(i16::MAX);
            if numbers.contains(&number) {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("duplicate column \"{name}\" in publication column list"),
                ));
            }
            numbers.push(number);
        }
        numbers.sort_unstable();
        Ok(numbers)
    }

    /// `check_publication_add_relation`.
    fn check_add_relation(&self, table: &PubTable<'s>) -> Result<()> {
        let qualified =
            format!("{}.{}", self.schema_name(self.found_namespace(table.found)), table.name);
        let rel = match table.found {
            Found::User(oid) => self.catalog.relation(oid),
            Found::Builtin(_) => None,
        };
        if table.except && rel.is_some_and(|r| r.partition_of.is_some()) {
            return Err(cannot_add(
                table,
                &qualified,
                "This operation is not supported for individual partitions.",
            ));
        }
        let kind = self.kind_of(table.found);
        if !matches!(kind, b'r' | b'p') {
            return Err(cannot_add(table, &qualified, relkind_detail(kind)));
        }
        if self.is_system(table.found) {
            return Err(cannot_add(
                table,
                &qualified,
                "This operation is not supported for system tables.",
            ));
        }
        let Some(rel) = rel else {
            return Err(not_yet("a publication of a built-in table", None));
        };
        if self.catalog.is_temp_schema(rel.namespace) {
            return Err(cannot_add(
                table,
                &qualified,
                "This operation is not supported for temporary tables.",
            ));
        }
        if rel.unlogged {
            return Err(cannot_add(
                table,
                &qualified,
                "This operation is not supported for unlogged tables.",
            ));
        }
        Ok(())
    }

    /// `PublicationAddTables` with `publication_add_relation`: the role must own each table.
    fn add_tables(
        &mut self,
        publication: &Publication,
        tables: &[PubTable<'s>],
        if_not_exists: bool,
    ) -> Result<()> {
        for table in tables {
            let owner = match table.found {
                Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
                Found::Builtin(_) => BOOTSTRAP_SUPERUSER,
            };
            if !self.is_superuser() && !self.owns(owner) {
                return Err(not_owner(owner_word(self.kind_of(table.found)), &table.name));
            }
            let oid = found_oid(table.found);
            if self.catalog.publication_rel_of(publication.oid, oid).is_some() {
                if if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "relation \"{}\" is already member of publication \"{}\"",
                        table.name, publication.name
                    ),
                ));
            }
            self.check_add_relation(table)?;
            let columns = self.publication_columns(table)?;
            let (qual, refs) = match &table.qual {
                Some(expr) => {
                    (Some(crate::node::write(expr)?), references(self.catalog, oid, &[expr])?)
                }
                None => (None, Vec::new()),
            };
            let attrs = (!table.columns.is_empty()).then_some(columns);
            self.catalog.add_publication_rel(
                PublicationRel {
                    oid: 0,
                    publication: publication.oid,
                    relation: oid,
                    except: table.except,
                    qual,
                    attrs,
                },
                &refs,
            );
        }
        Ok(())
    }

    /// `PublicationDropTables`.
    fn drop_tables(
        &mut self,
        publication: u32,
        tables: &[PubTable<'s>],
        missing_ok: bool,
    ) -> Result<()> {
        for table in tables {
            if !table.columns.is_empty() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "column list must not be specified in ALTER PUBLICATION ... DROP",
                ));
            }
            let oid = found_oid(table.found);
            let Some(row) = self.catalog.publication_rel_of(publication, oid).map(|r| r.oid) else {
                if missing_ok {
                    continue;
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("relation \"{}\" is not part of the publication", table.name),
                ));
            };
            if table.clause.is_some() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "cannot use a WHERE clause when removing a table from a publication",
                ));
            }
            self.perform(&[ObjRef::new(PG_PUBLICATION_REL, row)], true)?;
        }
        Ok(())
    }

    /// `check_publication_add_schema` and `publication_add_schema` for each schema.
    fn add_schemas(
        &mut self,
        publication: &Publication,
        schemas: &[u32],
        if_not_exists: bool,
    ) -> Result<()> {
        for &schema in schemas {
            let exists = self
                .catalog
                .publication_namespaces()
                .any(|n| n.publication == publication.oid && n.schema == schema);
            if exists {
                if if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "schema \"{}\" is already member of publication \"{}\"",
                        self.schema_name(schema),
                        publication.name
                    ),
                ));
            }
            let error = |detail: &str| {
                Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("cannot add schema \"{}\" to publication", self.schema_name(schema)),
                )
                .with_detail(detail)
            };
            if schema == PG_CATALOG_NAMESPACE || schema == PG_TOAST_NAMESPACE {
                return Err(error("This operation is not supported for system schemas."));
            }
            if self.catalog.is_temp_schema(schema) {
                return Err(error("Temporary schemas cannot be replicated."));
            }
            self.catalog.add_publication_namespace(publication.oid, schema);
        }
        Ok(())
    }

    /// `PublicationDropSchemas`.
    fn drop_schemas(&mut self, publication: u32, schemas: &[u32], missing_ok: bool) -> Result<()> {
        for &schema in schemas {
            let row = self
                .catalog
                .publication_namespaces()
                .find(|n| n.publication == publication && n.schema == schema)
                .map(|n| n.oid);
            let Some(row) = row else {
                if missing_ok {
                    continue;
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "tables from schema \"{}\" are not part of the publication",
                        self.schema_name(schema)
                    ),
                ));
            };
            self.perform(&[ObjRef::new(PG_PUBLICATION_NAMESPACE, row)], true)?;
        }
        Ok(())
    }

    /// The tables of the publication that are not in its `EXCEPT` clause, as `GetIncludedPublicationRelations` with `PUBLICATION_PART_ROOT` finds them, or the tables of the clause.
    fn publication_tables(&self, publication: u32, except: bool) -> Vec<u32> {
        self.catalog
            .publication_rels()
            .filter(|r| r.publication == publication && r.except == except)
            .map(|r| r.relation)
            .collect()
    }

    /// `is_schema_publication`.
    fn is_schema_publication(&self, publication: u32) -> bool {
        self.catalog.publication_namespaces().any(|n| n.publication == publication)
    }

    /// `CreatePublication`.
    pub(super) fn create_publication(&mut self, stmt: &'s CreatePublicationStmt) -> Result<()> {
        if !self.database_create_allowed(self.user) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.an.env.database()),
            ));
        }
        let name = stmt.pubname.as_deref().unwrap_or_default();
        if self.catalog.publication_by_name(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("publication \"{name}\" already exists"),
            ));
        }
        let options = publication_options(&stmt.options)?;
        if stmt.for_all_sequences && options.given() {
            self.notice(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "publication parameters are not applicable to sequence synchronization and will be ignored for sequences".to_string(),
            );
        }
        let [insert, update, delete, truncate] = options.publish.unwrap_or([true; 4]);
        let via_root = options.via_root.unwrap_or(false);
        let mut publication = Publication {
            oid: 0,
            name: name.to_string(),
            owner: self.user,
            all_tables: stmt.for_all_tables,
            all_sequences: stmt.for_all_sequences,
            insert,
            update,
            delete,
            truncate,
            via_root,
            gencols: options.gencols.unwrap_or(b'n'),
        };
        publication.oid = self.catalog.create_publication(publication.clone());
        let objects = self.publication_objects(&stmt.pubobjects)?;
        if stmt.for_all_tables {
            if !objects.except.is_empty() {
                let tables = self.open_tables(&objects.except)?;
                self.add_tables(&publication, &tables, true)?;
            }
        } else if !stmt.for_all_sequences {
            if !objects.tables.is_empty() {
                let mut tables = self.open_tables(&objects.tables)?;
                self.transform_filters(&mut tables, via_root)?;
                self.check_column_lists(name, &tables, !objects.schemas.is_empty(), via_root)?;
                self.add_tables(&publication, &tables, true)?;
            }
            self.add_schemas(&publication, &objects.schemas, true)?;
        }
        Ok(())
    }

    /// `AlterPublication`: the options, or the tables, the schemas and the flags of `FOR ALL`.
    pub(super) fn alter_publication(&mut self, stmt: &'s AlterPublicationStmt) -> Result<()> {
        let name = stmt.pubname.as_deref().unwrap_or_default();
        let publication = self.publication_named(name)?;
        if !self.is_superuser() && !self.owns(publication.owner) {
            return Err(not_owner("publication", name));
        }
        if !stmt.options.is_empty() {
            return self.alter_publication_options(stmt, &publication);
        }
        let objects = self.publication_objects(&stmt.pubobjects)?;
        self.check_alter_publication(stmt, &publication, &objects)?;
        let mut all: Vec<&'s PublicationTable> = objects.tables.clone();
        all.extend(objects.except.iter().copied());
        self.alter_publication_tables(stmt, &publication, &all, !objects.schemas.is_empty())?;
        self.alter_publication_schemas(stmt, &publication, &objects.schemas)?;
        if stmt.for_all_tables || stmt.for_all_sequences {
            let (tables, sequences) = (stmt.for_all_tables, stmt.for_all_sequences);
            self.catalog.alter_publication(publication.oid, |p| {
                p.all_tables = tables;
                p.all_sequences = sequences;
            });
        }
        Ok(())
    }

    /// `AlterPublicationOptions`.
    fn alter_publication_options(
        &mut self,
        stmt: &AlterPublicationStmt,
        publication: &Publication,
    ) -> Result<()> {
        let name = stmt.pubname.as_deref().unwrap_or_default();
        let options = publication_options(&stmt.options)?;
        if publication.all_sequences && options.given() {
            self.notice(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "publication parameters are not applicable to sequence synchronization and will be ignored for sequences".to_string(),
            );
        }
        if !publication.all_tables && options.via_root == Some(false) {
            for relation in self.publication_tables(publication.oid, false) {
                let Some(row) = self.catalog.publication_rel_of(publication.oid, relation) else {
                    continue;
                };
                let (filter, columns) = (row.qual.is_some(), row.attrs.is_some());
                if !filter && !columns {
                    continue;
                }
                if self.kind_of(Found::User(relation)) != b'p' {
                    continue;
                }
                let table = self.found_name(Found::User(relation));
                let what = if filter { "a WHERE clause" } else { "a column list" };
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!(
                        "cannot set parameter \"publish_via_partition_root\" to false for publication \"{name}\""
                    ),
                )
                .with_detail(format!(
                    "The publication contains {what} for partitioned table \"{table}\", which is not allowed when \"publish_via_partition_root\" is false."
                )));
            }
        }
        self.catalog.alter_publication(publication.oid, |p| {
            if let Some([insert, update, delete, truncate]) = options.publish {
                (p.insert, p.update, p.delete, p.truncate) = (insert, update, delete, truncate);
            }
            if let Some(via_root) = options.via_root {
                p.via_root = via_root;
            }
            if let Some(gencols) = options.gencols {
                p.gencols = gencols;
            }
        });
        Ok(())
    }

    /// `CheckAlterPublication`.
    fn check_alter_publication(
        &self,
        stmt: &AlterPublicationStmt,
        publication: &Publication,
        objects: &Objects<'s>,
    ) -> Result<()> {
        let superuser = self.is_superuser();
        let denied = |message: &str| Error::new(SqlState::INSUFFICIENT_PRIVILEGE, message);
        if matches!(
            stmt.action,
            AlterPublicationAction::AP_AddObjects | AlterPublicationAction::AP_SetObjects
        ) && !objects.schemas.is_empty()
            && !superuser
        {
            return Err(denied("must be superuser to add or set schemas"));
        }
        if stmt.for_all_tables && !superuser {
            return Err(denied("must be superuser to set ALL TABLES"));
        }
        if stmt.for_all_sequences && !superuser {
            return Err(denied("must be superuser to set ALL SEQUENCES"));
        }
        let all = match (publication.all_tables, publication.all_sequences) {
            (true, true) => Some("FOR ALL TABLES, ALL SEQUENCES"),
            (true, false) => Some("FOR ALL TABLES"),
            (false, true) => Some("FOR ALL SEQUENCES"),
            (false, false) => None,
        };
        if let Some(all) = all {
            let defined = |what: &str| {
                Error::new(
                    SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    format!("publication \"{}\" is defined as {all}", publication.name),
                )
                .with_detail(format!(
                    "{what} cannot be added to or dropped from {all} publications."
                ))
            };
            if !objects.schemas.is_empty() {
                return Err(defined("Schemas"));
            }
            if !objects.tables.is_empty() {
                return Err(defined("Tables or sequences"));
            }
        }
        if (stmt.for_all_tables || stmt.for_all_sequences)
            && (!self.publication_tables(publication.oid, false).is_empty()
                || self.is_schema_publication(publication.oid))
        {
            let what = if stmt.for_all_tables { "TABLES" } else { "SEQUENCES" };
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "publication \"{}\" does not support ALL {what} operations",
                    publication.name
                ),
            )
            .with_detail(
                "This operation requires the publication to be defined as FOR ALL TABLES/SEQUENCES or to be empty.",
            ));
        }
        Ok(())
    }

    /// `AlterPublicationTables`.
    fn alter_publication_tables(
        &mut self,
        stmt: &AlterPublicationStmt,
        publication: &Publication,
        tables: &[&'s PublicationTable],
        publish_schema: bool,
    ) -> Result<()> {
        let name = stmt.pubname.as_deref().unwrap_or_default();
        if tables.is_empty() && stmt.action != AlterPublicationAction::AP_SetObjects {
            return Ok(());
        }
        let mut rels = self.open_tables(tables)?;
        match stmt.action {
            AlterPublicationAction::AP_AddObjects => {
                self.transform_filters(&mut rels, publication.via_root)?;
                let publish_schema = publish_schema || self.is_schema_publication(publication.oid);
                self.check_column_lists(name, &rels, publish_schema, publication.via_root)?;
                self.add_tables(publication, &rels, false)
            }
            AlterPublicationAction::AP_DropObjects => {
                self.drop_tables(publication.oid, &rels, false)
            }
            _ => {
                let old = if stmt.for_all_tables || stmt.for_all_sequences {
                    if publication.all_tables {
                        self.publication_tables(publication.oid, true)
                    } else {
                        Vec::new()
                    }
                } else {
                    let old = self.publication_tables(publication.oid, false);
                    self.transform_filters(&mut rels, publication.via_root)?;
                    self.check_column_lists(name, &rels, publish_schema, publication.via_root)?;
                    old
                };
                let mut gone = Vec::new();
                for relation in old {
                    let row = self.catalog.publication_rel_of(publication.oid, relation).cloned();
                    let (old_qual, old_columns) = row.map_or((None, None), |r| (r.qual, r.attrs));
                    let mut found = false;
                    for rel in &rels {
                        let columns = self.publication_columns(rel)?;
                        if found_oid(rel.found) != relation {
                            continue;
                        }
                        let qual = rel.qual.as_ref().map(crate::node::write).transpose()?;
                        let columns = (!rel.columns.is_empty()).then_some(columns);
                        if qual == old_qual && columns == old_columns {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        gone.push(PubTable {
                            found: Found::User(relation),
                            name: self.found_name(Found::User(relation)),
                            clause: None,
                            qual: None,
                            columns: Vec::new(),
                            except: false,
                        });
                    }
                }
                self.drop_tables(publication.oid, &gone, true)?;
                self.add_tables(publication, &rels, true)
            }
        }
    }

    /// `AlterPublicationSchemas`.
    fn alter_publication_schemas(
        &mut self,
        stmt: &AlterPublicationStmt,
        publication: &Publication,
        schemas: &[u32],
    ) -> Result<()> {
        if schemas.is_empty() && stmt.action != AlterPublicationAction::AP_SetObjects {
            return Ok(());
        }
        match stmt.action {
            AlterPublicationAction::AP_AddObjects => {
                for relation in self.publication_tables(publication.oid, false) {
                    let columns = self
                        .catalog
                        .publication_rel_of(publication.oid, relation)
                        .is_some_and(|r| r.attrs.is_some());
                    if columns {
                        return Err(Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            format!(
                                "cannot add schema to publication \"{}\"",
                                stmt.pubname.as_deref().unwrap_or_default()
                            ),
                        )
                        .with_detail(
                            "Schemas cannot be added if any tables that specify a column list are already part of the publication.",
                        ));
                    }
                }
                self.add_schemas(publication, schemas, false)
            }
            AlterPublicationAction::AP_DropObjects => {
                self.drop_schemas(publication.oid, schemas, false)
            }
            _ => {
                let gone: Vec<u32> = self
                    .catalog
                    .publication_namespaces()
                    .filter(|n| n.publication == publication.oid && !schemas.contains(&n.schema))
                    .map(|n| n.schema)
                    .collect();
                self.drop_schemas(publication.oid, &gone, true)?;
                self.add_schemas(publication, schemas, true)
            }
        }
    }

    /// The publication of `COMMENT ON PUBLICATION`, which the role must own.
    pub(super) fn comment_publication(&self, object: Option<&Node>) -> Result<ObjRef> {
        let name = match object {
            Some(Node::String(name)) => &**name,
            _ => "",
        };
        let publication = self.publication_named(name)?;
        if !self.is_superuser() && !self.owns(publication.owner) {
            return Err(not_owner("publication", name));
        }
        Ok(ObjRef::new(PG_PUBLICATION, publication.oid))
    }

    /// `AlterObjectRename_internal` for a publication.
    pub(super) fn rename_publication(&mut self, stmt: &RenameStmt) -> Result<()> {
        let found = self.comment_publication(stmt.object.as_ref())?;
        let newname = stmt.newname.as_deref().unwrap_or_default();
        if self.catalog.publication_by_name(newname).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("publication \"{newname}\" already exists"),
            ));
        }
        let newname = newname.to_string();
        self.catalog.alter_publication(found.oid, |p| p.name = newname);
        Ok(())
    }

    /// `AlterPublicationOwner_internal`.
    pub(super) fn alter_publication_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let name = match &stmt.object {
            Some(Node::String(name)) => name.to_string(),
            _ => String::new(),
        };
        let publication = self.publication_named(&name)?;
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        self.change_publication_owner(&publication, new)
    }

    /// The checks of `AlterPublicationOwner_internal` and the change of the owner.
    pub(super) fn change_publication_owner(
        &mut self,
        publication: &Publication,
        new: u32,
    ) -> Result<()> {
        if publication.owner == new {
            return Ok(());
        }
        let roles = rupg_pgcatalog::roles::current();
        if !self.is_superuser() {
            if !self.owns(publication.owner) {
                return Err(not_owner("publication", &publication.name));
            }
            self.check_can_set_role(new)?;
            if !roles.superuser(new)
                && (publication.all_tables
                    || publication.all_sequences
                    || self.is_schema_publication(publication.oid))
            {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!(
                        "permission denied to change owner of publication \"{}\"",
                        publication.name
                    ),
                )
                .with_hint(
                    "The owner of a FOR ALL TABLES or ALL SEQUENCES or TABLES IN SCHEMA publication must be a superuser.",
                ));
            }
            if !self.database_create_allowed(new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for database {}", self.an.env.database()),
                ));
            }
        }
        let (old, new_name) = (
            roles.name(publication.owner).unwrap_or_default().to_string(),
            roles.name(new).unwrap_or_default().to_string(),
        );
        self.catalog.set_object_owner(
            ObjRef::new(PG_PUBLICATION, publication.oid),
            new,
            &old,
            &new_name,
        )
    }

    /// `getObjectDescription` for a publication, a table of a publication and a schema of a publication.
    pub(super) fn describe_publication(&self, object: ObjRef) -> String {
        let name = |oid: u32| self.catalog.publication(oid).map_or("", |p| p.name.as_str());
        match object.class {
            PG_PUBLICATION_REL => match self.catalog.publication_rel(object.oid) {
                Some(row) if row.except => format!(
                    "exclusion of {} from publication {}",
                    self.describe_relation(row.relation),
                    name(row.publication)
                ),
                Some(row) => format!(
                    "publication of {} in publication {}",
                    self.describe_relation(row.relation),
                    name(row.publication)
                ),
                None => format!("publication table {}", object.oid),
            },
            PG_PUBLICATION_NAMESPACE => match self.catalog.publication_namespace(object.oid) {
                Some(row) => format!(
                    "publication of schema {} in publication {}",
                    self.schema_name(row.schema),
                    name(row.publication)
                ),
                None => format!("publication schema {}", object.oid),
            },
            _ => format!("publication {}", name(object.oid)),
        }
    }
}

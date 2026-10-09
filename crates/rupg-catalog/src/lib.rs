//! The rupg catalog of user objects: schemas, tables, columns, constraints, indexes, roles, privileges.
//!
//! At M2 the catalog holds the schemas, tables, sequences, indexes, views, column defaults and constraints that DDL makes. Each object gets its OID in the order that PostgreSQL 19 gives OIDs, and each object without a name in the statement gets the name that PostgreSQL gives it. Thus after the same statements on a new cluster, the OIDs and the names are the same as in PostgreSQL.
//!
//! The catalog does not read SQL. The analyzer turns a statement into calls of this crate in the order of `DefineRelation`, `DefineIndex` and `ATAddForeignKeyConstraint`, and it gives each expression as text that only the analyzer reads back. The catalog makes the TOAST tables that PostgreSQL makes, with their indexes, but they hold no data. It does not make the triggers of foreign keys, but it uses their OIDs, so the OIDs of later objects agree with PostgreSQL. It records no `pg_depend` rows for these triggers. The catalog also records the rows of `pg_depend` in the order that PostgreSQL inserts them.
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

pub mod names;
pub mod toast;

use std::collections::BTreeMap;

use rupg_common::{Error, Result, SqlState};

use names::{index_column_names, make_object_name, name_addition};

/// The first OID for a user object, `FirstNormalObjectId`.
pub const FIRST_NORMAL_OID: u32 = 16384;
/// `FirstUnpinnedObjectId`. Most objects with a lower OID are pinned, and no `pg_depend` row refers to them. See [`is_pinned`].
pub const FIRST_UNPINNED_OID: u32 = 12000;
/// `MaxHeapAttributeNumber`.
pub const MAX_COLUMNS: usize = 1600;
/// The OID of `pg_class`, the catalog of relations.
pub const PG_CLASS: u32 = 1259;
/// The OID of `pg_type`.
pub const PG_TYPE: u32 = 1247;
/// The OID of `pg_rewrite`.
pub const PG_REWRITE: u32 = 2618;
/// The OID of `pg_proc`.
pub const PG_PROC: u32 = 1255;
/// The OID of `pg_namespace`.
pub const PG_NAMESPACE: u32 = 2615;
/// The OID of `pg_database`.
pub const PG_DATABASE: u32 = 1262;
/// The OID of `pg_largeobject`.
pub const PG_LARGEOBJECT: u32 = 2613;
/// The OID of the schema `pg_toast`.
pub const PG_TOAST_NAMESPACE: u32 = 99;
/// The OID of the type `oid`.
const OID: u32 = 26;
/// The OID of the type `int4`.
const INT4: u32 = 23;
/// The OID of the type `bytea`.
const BYTEA: u32 = 17;
/// The OID of the operator class `oid_ops` of `btree`.
const OID_OPS: u32 = 1981;
/// The OID of the operator class `int4_ops` of `btree`.
const INT4_OPS: u32 = 1978;
/// The OID of the schema `public`.
pub const PUBLIC: u32 = 2200;
/// The OID of `pg_constraint`.
pub const PG_CONSTRAINT: u32 = 2606;
/// The OID of `pg_attrdef`.
pub const PG_ATTRDEF: u32 = 2604;
/// The OID of the catalog `pg_collation`.
pub const PG_COLLATION: u32 = 3456;
/// The OID of the `btree` access method.
pub const BTREE: u32 = 403;
/// The number of triggers that PostgreSQL makes for one foreign key.
const FOREIGN_KEY_TRIGGERS: u32 = 4;

/// A schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// `nspacl`: the privileges as `aclitem` texts, or `None` for the default privileges of the owner.
    pub acl: Option<Vec<String>>,
}

/// The kind of a relation, `relkind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelKind {
    /// An ordinary table, `r`.
    Table,
    /// An index, `i`.
    Index,
    /// A sequence, `S`.
    Sequence,
    /// A TOAST table, `t`.
    Toast,
    /// A view, `v`.
    View,
}

impl RelKind {
    /// The `relkind` letter.
    pub fn code(self) -> char {
        match self {
            RelKind::Table => 'r',
            RelKind::Index => 'i',
            RelKind::Sequence => 'S',
            RelKind::Toast => 't',
            RelKind::View => 'v',
        }
    }
}

/// A column of a relation, one row of `pg_attribute` with a positive `attnum`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    /// The name.
    pub name: String,
    /// The OID of the type.
    pub ty: u32,
    /// The type modifier, or -1.
    pub typmod: i32,
    /// The number of array dimensions that the statement gives, `attndims`.
    pub ndims: i16,
    /// The OID of the collation, or 0.
    pub collation: u32,
    /// `attnotnull`.
    pub not_null: bool,
    /// `atthasdef`.
    pub has_default: bool,
}

impl Column {
    /// A column with no constraint and no default.
    pub fn new(name: impl Into<String>, ty: u32, typmod: i32, collation: u32) -> Column {
        Column {
            name: name.into(),
            ty,
            typmod,
            ndims: 0,
            collation,
            not_null: false,
            has_default: false,
        }
    }
}

/// The facts of an index, one row of `pg_index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexInfo {
    /// The OID of the table.
    pub table: u32,
    /// The column number of each index column in the table, or 0 for an expression.
    pub keys: Vec<i16>,
    /// The number of key columns. The rest are `INCLUDE` columns.
    pub key_count: i16,
    /// `indisunique`.
    pub unique: bool,
    /// `indnullsnotdistinct`.
    pub nulls_not_distinct: bool,
    /// `indisprimary`.
    pub primary: bool,
    /// `indimmediate`: false for a deferrable constraint.
    pub immediate: bool,
    /// The OID of the access method.
    pub method: u32,
    /// The collation of each key column, `indcollation`.
    pub collations: Vec<u32>,
    /// The operator class of each key column, `indclass`.
    pub classes: Vec<u32>,
    /// The options of each key column, `indoption`.
    pub options: Vec<i16>,
    /// The expressions of the index columns with key 0, in the form of the analyzer.
    pub exprs: Option<String>,
    /// The predicate of a partial index, in the form of the analyzer.
    pub predicate: Option<String>,
}

/// The facts of a sequence, one row of `pg_sequence`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SequenceInfo {
    /// The OID of the type of the values.
    pub ty: u32,
    /// The first value.
    pub start: i64,
    /// The step.
    pub increment: i64,
    /// The largest value.
    pub max: i64,
    /// The smallest value.
    pub min: i64,
    /// The number of values to cache.
    pub cache: i64,
    /// True when the sequence starts again after its last value.
    pub cycle: bool,
}

/// A relation: a table, an index, a sequence or a TOAST table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The kind.
    pub kind: RelKind,
    /// The OID of the owner role.
    pub owner: u32,
    /// The OID of the row type, or 0 for an index or a sequence.
    pub row_type: u32,
    /// The columns, in the order of their numbers.
    pub columns: Vec<Column>,
    /// `relhasindex`: true after the first index on the table.
    pub has_index: bool,
    /// `relhastriggers`: true after the first foreign key that refers to the table or that the table has.
    pub has_triggers: bool,
    /// `reltoastrelid`: the OID of the TOAST table, or 0.
    pub toast: u32,
    /// The facts of an index.
    pub index: Option<IndexInfo>,
    /// The facts of a sequence.
    pub sequence: Option<SequenceInfo>,
    /// The query of a view.
    pub view: Option<ViewInfo>,
    /// `reloptions`: each option as `name=value`.
    pub options: Vec<String>,
    /// `relacl`: the items of the `aclitem[]` value in their text form, or `None` for the default privileges of the owner.
    pub acl: Option<Vec<String>>,
}

/// The query of a view and its `_RETURN` rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewInfo {
    /// The OID of the `_RETURN` rule in `pg_rewrite`.
    pub rule: u32,
    /// The text of the statement that made the view or that last replaced its query. Only the analyzer reads it back.
    pub text: String,
    /// The schemas of `search_path` when that statement ran, in order. The analyzer finds the names of the query in these schemas again.
    pub path: Vec<u32>,
}

impl Relation {
    /// The number of the column with the name, from 1.
    pub fn column_number(&self, name: &str) -> Option<i16> {
        let index = self.columns.iter().position(|c| c.name == name)?;
        i16::try_from(index + 1).ok()
    }

    /// The column with the number, from 1.
    pub fn column(&self, number: i16) -> Option<&Column> {
        let index = usize::try_from(number).ok()?.checked_sub(1)?;
        self.columns.get(index)
    }
}

/// A row type, a domain, or the array type of one of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// The OID of the relation of a row type, or 0 for an array type.
    pub relation: u32,
    /// The OID of the element type of an array type, or 0 for a row type.
    pub element: u32,
    /// The OID of the array type of a row type or a domain, or 0 for an array type.
    pub array: u32,
    /// The facts of a domain, or `None` for a type of another kind.
    pub domain: Option<Domain>,
}

/// The facts of a domain that `pg_type` holds for a type of the kind `d`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Domain {
    /// `typbasetype`: the OID of the base type.
    pub base: u32,
    /// `typtypmod`: the typmod that the domain applies to the base type, or -1.
    pub typmod: i32,
    /// `typcollation`: the collation of the domain, or 0 for a type that is not collatable.
    pub collation: u32,
    /// `typdefaultbin`: the default in the form of the analyzer, or `None`.
    pub default: Option<String>,
}

/// A function, one row of `pg_proc`. Only the setup of a new cluster makes functions, for the functions in SQL of `information_schema`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// The OID. [`Catalog::create_function`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner role.
    pub owner: u32,
    /// `prolang`: the OID of the language.
    pub lang: u32,
    /// `procost`: the cost in units of `cpu_operator_cost`.
    pub cost: u32,
    /// `prorows`: the number of rows of a function that returns a set, or 0.
    pub rows: u32,
    /// `prosupport`: the OID of the support function, or 0.
    pub support: u32,
    /// `proisstrict`: true when a null argument gives null without a call.
    pub strict: bool,
    /// `provolatile`: `i` immutable, `s` stable or `v` volatile.
    pub volatile: u8,
    /// `proparallel`: `s` safe, `r` restricted or `u` unsafe.
    pub parallel: u8,
    /// `prorettype`: the OID of the result type.
    pub rettype: u32,
    /// `proretset`: true when the function returns a set.
    pub retset: bool,
    /// `proargtypes`: the types of the input arguments.
    pub argtypes: Vec<u32>,
    /// `proallargtypes`: the types of all the arguments, with the `OUT` arguments. The list is empty when all the arguments are input arguments.
    pub allargtypes: Vec<u32>,
    /// `proargmodes`: the mode of each argument, `i`, `o`, `b` or `t`. The list is empty when all the arguments are input arguments.
    pub argmodes: Vec<u8>,
    /// `proargnames`: the name of each argument, with an empty name for an argument without a name. The list is empty when no argument has a name.
    pub argnames: Vec<String>,
    /// `prosrc`: the body in a string, or empty for a body in SQL.
    pub src: String,
    /// The text of the statement that made the function, or `None`.
    pub body: Option<FunctionBody>,
}

/// The body of a function in SQL, which the analyzer reads again each time a query calls the function, as it reads the query of a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionBody {
    /// The text of the statement that made the function.
    pub text: String,
    /// The schemas of `search_path` when that statement ran, in order.
    pub path: Vec<u32>,
}

/// A column default, one row of `pg_attrdef`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttrDefault {
    /// The OID.
    pub oid: u32,
    /// The OID of the relation.
    pub relation: u32,
    /// The number of the column.
    pub column: i16,
    /// The expression, in the form of the analyzer.
    pub expr: String,
}

/// The kind of a constraint, `contype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConKind {
    /// A check constraint, `c`.
    Check,
    /// A not-null constraint, `n`.
    NotNull,
    /// A primary key, `p`.
    Primary,
    /// A unique constraint, `u`.
    Unique,
    /// A foreign key, `f`.
    Foreign,
}

impl ConKind {
    /// The `contype` letter.
    pub fn code(self) -> char {
        match self {
            ConKind::Check => 'c',
            ConKind::NotNull => 'n',
            ConKind::Primary => 'p',
            ConKind::Unique => 'u',
            ConKind::Foreign => 'f',
        }
    }
}

/// The facts of a foreign key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    /// The OID of the referenced table.
    pub table: u32,
    /// The referenced columns, `confkey`.
    pub keys: Vec<i16>,
    /// `confupdtype`: `a`, `r`, `c`, `n` or `d`.
    pub update: char,
    /// `confdeltype`.
    pub delete: char,
    /// `confmatchtype`: `s`, `f` or `p`.
    pub match_type: char,
    /// `conpfeqop`.
    pub pf_eq: Vec<u32>,
    /// `conppeqop`.
    pub pp_eq: Vec<u32>,
    /// `conffeqop`.
    pub ff_eq: Vec<u32>,
}

/// A constraint, one row of `pg_constraint`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Constraint {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The kind.
    pub kind: ConKind,
    /// The OID of the relation, or 0 for a constraint of a domain.
    pub relation: u32,
    /// `contypid`: the OID of the domain of a domain constraint, else 0.
    pub domain: u32,
    /// The constrained columns, `conkey`.
    pub keys: Vec<i16>,
    /// The OID of the index of a primary key, a unique constraint or a foreign key, else 0.
    pub index: u32,
    /// `condeferrable`.
    pub deferrable: bool,
    /// `condeferred`.
    pub deferred: bool,
    /// `convalidated`.
    pub validated: bool,
    /// `connoinherit`.
    pub no_inherit: bool,
    /// The expression of a check constraint, in the form of the analyzer.
    pub expr: Option<String>,
    /// The facts of a foreign key.
    pub foreign: Option<ForeignKey>,
}

/// An object as `pg_depend` names it: the OID of its catalog, its OID and a column number or 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjRef {
    /// The OID of the catalog that holds the object.
    pub class: u32,
    /// The OID of the object.
    pub oid: u32,
    /// The column number, or 0 for the whole object.
    pub sub: i32,
}

impl ObjRef {
    /// A whole object.
    pub fn new(class: u32, oid: u32) -> ObjRef {
        ObjRef { class, oid, sub: 0 }
    }

    /// A column of a relation.
    pub fn column(relation: u32, number: i16) -> ObjRef {
        ObjRef { class: PG_CLASS, oid: relation, sub: i32::from(number) }
    }
}

/// The kind of a dependency, `deptype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DepKind {
    /// `n`: the dependent object needs `CASCADE` to go with the referenced object.
    Normal,
    /// `a`: the dependent object goes with the referenced object.
    Auto,
    /// `i`: the dependent object is a part of the referenced object.
    Internal,
}

impl DepKind {
    /// The `deptype` letter.
    pub fn code(self) -> char {
        match self {
            DepKind::Normal => 'n',
            DepKind::Auto => 'a',
            DepKind::Internal => 'i',
        }
    }
}

/// One row of `pg_depend`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Depend {
    /// The dependent object.
    pub object: ObjRef,
    /// The referenced object.
    pub referenced: ObjRef,
    /// The kind.
    pub kind: DepKind,
}

/// A new domain for [`Catalog::create_domain`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDomain {
    /// The OID of the schema.
    pub namespace: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// The facts of the domain.
    pub domain: Domain,
}

/// A new table, a new index or a new sequence before it has an OID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRelation {
    /// The OID of the schema.
    pub namespace: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// The columns.
    pub columns: Vec<Column>,
}

/// A column of a new index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIndexColumn {
    /// The name that the statement gives the index column, else the name of the table column, else `expr`.
    pub name: String,
    /// The number of the table column, or 0 for an expression.
    pub key: i16,
    /// The type of the index column.
    pub ty: u32,
    /// The type modifier of the index column.
    pub typmod: i32,
    /// The collation, or 0.
    pub collation: u32,
    /// The operator class, or 0 for an `INCLUDE` column.
    pub class: u32,
    /// The `indoption` bits.
    pub option: i16,
}

/// A new index before it has an OID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewIndex {
    /// The OID of the table.
    pub table: u32,
    /// The name that the statement gives, or `None` to make one.
    pub name: Option<String>,
    /// The columns: the key columns, then the `INCLUDE` columns.
    pub columns: Vec<NewIndexColumn>,
    /// The number of key columns.
    pub key_count: usize,
    /// The OID of the access method.
    pub method: u32,
    /// True for a unique index.
    pub unique: bool,
    /// True for `NULLS NOT DISTINCT`.
    pub nulls_not_distinct: bool,
    /// The constraint that the index is for: `Primary`, `Unique` or `None`.
    pub constraint: Option<ConKind>,
    /// True for a deferrable constraint.
    pub deferrable: bool,
    /// True for a constraint that is initially deferred.
    pub deferred: bool,
    /// The expressions of the index columns with key 0, in the form of the analyzer.
    pub exprs: Option<String>,
    /// The predicate, in the form of the analyzer.
    pub predicate: Option<String>,
    /// The objects that the expressions and the predicate refer to.
    pub refs: Vec<ObjRef>,
}

/// A new foreign key before it has an OID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewForeignKey {
    /// The OID of the table.
    pub table: u32,
    /// The name that the statement gives, or `None` to make one.
    pub name: Option<String>,
    /// The referencing columns.
    pub keys: Vec<i16>,
    /// The OID of the unique index of the referenced table that the key uses.
    pub index: u32,
    /// True for a deferrable constraint.
    pub deferrable: bool,
    /// True for a constraint that is initially deferred.
    pub deferred: bool,
    /// The referenced table, its columns, the actions and the operators.
    pub foreign: ForeignKey,
}

/// The objects of one cluster, with the OID counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    next_oid: u32,
    schemas: BTreeMap<u32, Schema>,
    relations: BTreeMap<u32, Relation>,
    types: BTreeMap<u32, Type>,
    defaults: BTreeMap<u32, AttrDefault>,
    constraints: BTreeMap<u32, Constraint>,
    functions: BTreeMap<u32, Function>,
    depends: Vec<Depend>,
}

impl Default for Catalog {
    fn default() -> Catalog {
        Catalog::new()
    }
}

/// `IsPinnedObject`: true for an object that initdb made, except the schema `public`, the databases and the large objects.
pub fn is_pinned(object: ObjRef) -> bool {
    if object.oid >= FIRST_UNPINNED_OID
        || object.class == PG_LARGEOBJECT
        || object.class == PG_DATABASE
    {
        return false;
    }
    !(object.class == PG_NAMESPACE && object.oid == PUBLIC)
}

fn duplicate_table(name: &str) -> Error {
    Error::new(SqlState::DUPLICATE_TABLE, format!("relation \"{name}\" already exists"))
}

fn duplicate_constraint(name: &str, relation: &str) -> Error {
    Error::new(
        SqlState::DUPLICATE_OBJECT,
        format!("constraint \"{name}\" for relation \"{relation}\" already exists"),
    )
}

fn no_object(what: &str, oid: u32) -> Error {
    Error::internal(format!("cache lookup failed for {what} {oid}"))
}

impl Catalog {
    /// An empty catalog. The first object gets the OID 16384.
    pub fn new() -> Catalog {
        Catalog {
            next_oid: FIRST_NORMAL_OID,
            schemas: BTreeMap::new(),
            relations: BTreeMap::new(),
            types: BTreeMap::new(),
            defaults: BTreeMap::new(),
            constraints: BTreeMap::new(),
            functions: BTreeMap::new(),
            depends: Vec::new(),
        }
    }

    /// The OID that the next object gets.
    pub fn next_oid(&self) -> u32 {
        self.next_oid
    }

    /// Moves the OID counter forward to `oid`. PostgreSQL does not give an OID again after a rollback, so a session gives the counter of a rolled back copy to the copy that it keeps.
    pub fn advance_oid(&mut self, oid: u32) {
        self.next_oid = self.next_oid.max(oid);
    }

    /// Sets the OID counter. Only `initdb` gives the OIDs below 16384, so only the catalog of a new cluster sets the counter back.
    pub fn set_next_oid(&mut self, oid: u32) {
        self.next_oid = oid;
    }

    /// Uses `count` OIDs for objects that PostgreSQL makes and rupg does not, such as the triggers of a foreign key.
    pub fn skip_oids(&mut self, count: u32) {
        for _ in 0..count {
            self.new_oid();
        }
    }

    fn new_oid(&mut self) -> u32 {
        let oid = self.next_oid;
        self.next_oid = match oid.checked_add(1) {
            Some(next) => next,
            None => FIRST_NORMAL_OID,
        };
        oid
    }

    fn depend(&mut self, object: ObjRef, referenced: ObjRef, kind: DepKind) {
        if is_pinned(referenced) {
            return;
        }
        self.depends.push(Depend { object, referenced, kind });
    }

    /// The dependencies on the objects that an expression refers to, as `recordDependencyOnSingleRelExpr` makes them. A reference to a column of `relation` gets `self_kind`, and each other reference gets `Normal`.
    fn depend_on_expr(
        &mut self,
        object: ObjRef,
        refs: &[ObjRef],
        relation: u32,
        self_kind: DepKind,
    ) {
        for &referenced in refs {
            let kind = if referenced.class == PG_CLASS && referenced.oid == relation {
                self_kind
            } else {
                DepKind::Normal
            };
            self.depend(object, referenced, kind);
        }
    }

    /// The user schemas.
    pub fn schemas(&self) -> impl Iterator<Item = &Schema> {
        self.schemas.values()
    }

    /// The schema with the OID.
    pub fn schema(&self, oid: u32) -> Option<&Schema> {
        self.schemas.get(&oid)
    }

    /// The user schema with the name.
    pub fn schema_by_name(&self, name: &str) -> Option<&Schema> {
        self.schemas.values().find(|s| s.name == name)
    }

    /// The relations.
    pub fn relations(&self) -> impl Iterator<Item = &Relation> {
        self.relations.values()
    }

    /// The relation with the OID.
    pub fn relation(&self, oid: u32) -> Option<&Relation> {
        self.relations.get(&oid)
    }

    /// The relation with the name in the schema.
    pub fn relation_by_name(&self, namespace: u32, name: &str) -> Option<&Relation> {
        self.relations.values().find(|r| r.namespace == namespace && r.name == name)
    }

    /// The row types and array types.
    pub fn types(&self) -> impl Iterator<Item = &Type> {
        self.types.values()
    }

    /// The type with the OID.
    pub fn type_by_oid(&self, oid: u32) -> Option<&Type> {
        self.types.get(&oid)
    }

    /// The type with the name in the schema.
    pub fn type_by_name(&self, namespace: u32, name: &str) -> Option<&Type> {
        self.types.values().find(|t| t.namespace == namespace && t.name == name)
    }

    /// The column defaults.
    pub fn defaults(&self) -> impl Iterator<Item = &AttrDefault> {
        self.defaults.values()
    }

    /// The default of a column.
    pub fn default_of(&self, relation: u32, column: i16) -> Option<&AttrDefault> {
        self.defaults.values().find(|d| d.relation == relation && d.column == column)
    }

    /// The constraints.
    pub fn constraints(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints.values()
    }

    /// The constraint with the OID.
    pub fn constraint(&self, oid: u32) -> Option<&Constraint> {
        self.constraints.get(&oid)
    }

    /// The constraints of a relation, in the order of their OIDs.
    pub fn constraints_of(&self, relation: u32) -> impl Iterator<Item = &Constraint> {
        self.constraints.values().filter(move |c| c.relation == relation)
    }

    /// The check constraints of a domain.
    pub fn constraints_of_domain(&self, domain: u32) -> impl Iterator<Item = &Constraint> {
        self.constraints.values().filter(move |c| c.domain == domain)
    }

    /// The functions in the order of their OIDs.
    pub fn functions(&self) -> impl Iterator<Item = &Function> {
        self.functions.values()
    }

    /// The function with the OID.
    pub fn function(&self, oid: u32) -> Option<&Function> {
        self.functions.get(&oid)
    }

    /// The rows of `pg_depend` for user objects, in the order that PostgreSQL inserts them.
    pub fn depends(&self) -> &[Depend] {
        &self.depends
    }

    /// `ConstraintNameExists`: true when a constraint in the schema has the name.
    fn constraint_name_used(&self, namespace: u32, name: &str) -> bool {
        self.constraints.values().any(|c| c.namespace == namespace && c.name == name)
    }

    /// `ChooseRelationName`: `make_object_name(name1, name2, label)`, with a number after the label when a relation in the schema has the name, or a constraint in the schema has it and `constraint` is true.
    pub fn choose_relation_name(
        &self,
        name1: &str,
        name2: Option<&str>,
        label: &str,
        namespace: u32,
        constraint: bool,
    ) -> String {
        let mut pass = 0;
        let mut modlabel = label.to_string();
        loop {
            let name = make_object_name(name1, name2, Some(&modlabel));
            let collides = self.relation_by_name(namespace, &name).is_some()
                || (constraint && self.constraint_name_used(namespace, &name));
            if !collides {
                return name;
            }
            pass += 1;
            modlabel = format!("{label}{pass}");
        }
    }

    /// `ChooseConstraintName`: as [`Catalog::choose_relation_name`], but only the constraints of the schema count.
    pub fn choose_constraint_name(
        &self,
        name1: &str,
        name2: Option<&str>,
        label: &str,
        namespace: u32,
    ) -> String {
        let mut pass = 0;
        let mut modlabel = if label.is_empty() {
            pass += 1;
            format!("{label}{pass}")
        } else {
            label.to_string()
        };
        loop {
            let name = make_object_name(name1, name2, Some(&modlabel));
            if !self.constraint_name_used(namespace, &name) {
                return name;
            }
            pass += 1;
            modlabel = format!("{label}{pass}");
        }
    }

    /// `makeArrayTypeName`: `_name`, with `_` and a number after it when a type in the schema has the name.
    fn array_type_name(&self, name: &str, namespace: u32) -> String {
        let mut array = make_object_name("", Some(name), None);
        let mut pass = 0;
        while self.type_by_name(namespace, &array).is_some() {
            pass += 1;
            array = make_object_name("", Some(name), Some(&pass.to_string()));
        }
        array
    }

    /// `CREATE SCHEMA`. The caller checks the names of the built-in schemas and the `pg_` prefix.
    pub fn create_schema(&mut self, name: &str, owner: u32) -> Result<u32> {
        if self.schema_by_name(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists"),
            ));
        }
        let oid = self.new_oid();
        self.schemas.insert(oid, Schema { oid, name: name.to_string(), owner, acl: None });
        Ok(oid)
    }

    /// Checks the name of a new relation, as `heap_create_with_catalog` and `index_create` do. When the row type of a new table would have the name of an array type that PostgreSQL made, the array type gets a new name, as `moveArrayTypeName` gives it.
    fn check_new_relation(&mut self, namespace: u32, name: &str, row_type: bool) -> Result<()> {
        if self.relation_by_name(namespace, name).is_some() {
            return Err(duplicate_table(name));
        }
        if !row_type {
            return Ok(());
        }
        self.check_new_type(namespace, name)
    }

    /// Checks the name of a new type: when the name is the name of an array type that PostgreSQL made, the array type gets a new name, as `moveArrayTypeName` gives it. Any other type with the name is an error.
    fn check_new_type(&mut self, namespace: u32, name: &str) -> Result<()> {
        let Some(old) = self.type_by_name(namespace, name).map(|t| (t.oid, t.element)) else {
            return Ok(());
        };
        let is_auto_array = old.1 != 0 && self.types.get(&old.1).is_some_and(|e| e.array == old.0);
        if !is_auto_array {
            return Err(Error::new(SqlState::DUPLICATE_OBJECT, format!("type \"{name}\" already exists")).with_hint(
                "A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.",
            ));
        }
        let renamed = self.array_type_name(name, namespace);
        if let Some(array) = self.types.get_mut(&old.0) {
            array.name = renamed;
        }
        Ok(())
    }

    fn check_columns(columns: &[Column]) -> Result<()> {
        if columns.len() > MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {MAX_COLUMNS} columns"),
            ));
        }
        for (i, column) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.name == column.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", column.name),
                ));
            }
        }
        Ok(())
    }

    /// `heap_create_with_catalog` for an ordinary table: the table gets an OID, then its array type, then its row type.
    pub fn create_table(&mut self, new: NewRelation) -> Result<u32> {
        self.create_relation(new, RelKind::Table)
    }

    /// `heap_create_with_catalog` for a relation with a row type: the relation gets an OID, then its array type, then its row type. Each column depends on its type and its collation, as `AddNewAttributeTuples` records them.
    fn create_relation(&mut self, new: NewRelation, kind: RelKind) -> Result<u32> {
        Catalog::check_columns(&new.columns)?;
        self.check_new_relation(new.namespace, &new.name, true)?;
        let oid = self.new_oid();
        let array = self.new_oid();
        let row_type = self.new_oid();
        let array_name = self.array_type_name(&new.name, new.namespace);
        let column_refs: Vec<(ObjRef, ObjRef)> = (1i16..)
            .zip(&new.columns)
            .flat_map(|(n, c)| {
                let types = [(PG_TYPE, c.ty), (PG_COLLATION, c.collation)];
                types
                    .into_iter()
                    .filter(|&(_, found)| found != 0)
                    .map(move |(class, found)| (ObjRef::column(oid, n), ObjRef::new(class, found)))
            })
            .collect();
        self.types.insert(
            row_type,
            Type {
                oid: row_type,
                name: new.name.clone(),
                namespace: new.namespace,
                owner: new.owner,
                relation: oid,
                element: 0,
                array,
                domain: None,
            },
        );
        self.types.insert(
            array,
            Type {
                oid: array,
                name: array_name,
                namespace: new.namespace,
                owner: new.owner,
                relation: 0,
                element: row_type,
                array: 0,
                domain: None,
            },
        );
        self.relations.insert(
            oid,
            Relation {
                oid,
                name: new.name,
                namespace: new.namespace,
                kind,
                owner: new.owner,
                row_type,
                columns: new.columns,
                has_index: false,
                has_triggers: false,
                toast: 0,
                index: None,
                sequence: None,
                view: None,
                options: Vec::new(),
                acl: None,
            },
        );
        self.depend(ObjRef::new(PG_TYPE, row_type), ObjRef::new(PG_CLASS, oid), DepKind::Internal);
        self.depend(ObjRef::new(PG_TYPE, array), ObjRef::new(PG_TYPE, row_type), DepKind::Internal);
        for (column, found) in column_refs {
            self.depend(column, found, DepKind::Normal);
        }
        self.depend(
            ObjRef::new(PG_CLASS, oid),
            ObjRef::new(PG_NAMESPACE, new.namespace),
            DepKind::Normal,
        );
        Ok(oid)
    }

    /// `DefineView`: the view gets its OID, its array type and its row type as a table does, then its `_RETURN` rule. The rule is part of the view, and it depends on the objects that the query reads.
    pub fn create_view(
        &mut self,
        new: NewRelation,
        text: String,
        path: Vec<u32>,
        refs: &[ObjRef],
    ) -> Result<u32> {
        let oid = self.create_relation(new, RelKind::View)?;
        let rule = self.new_oid();
        self.relation_mut(oid)?.view = Some(ViewInfo { rule, text, path });
        self.depend_rule(rule, oid, refs);
        Ok(oid)
    }

    /// `CREATE OR REPLACE VIEW` of a view that exists. The columns after the old columns are new, and the rule gets the new query and new dependencies. The caller checks that the old columns did not change.
    pub fn replace_view(
        &mut self,
        oid: u32,
        columns: Vec<Column>,
        text: String,
        path: Vec<u32>,
        refs: &[ObjRef],
    ) -> Result<()> {
        Catalog::check_columns(&columns)?;
        let rel = self.relation_mut(oid)?;
        let old = rel.columns.len();
        rel.columns.extend(columns.into_iter().skip(old));
        let view = rel.view.as_mut().ok_or_else(|| no_object("view", oid))?;
        view.text = text;
        view.path = path;
        let rule = view.rule;
        self.depends.retain(|d| d.object != ObjRef::new(PG_REWRITE, rule));
        self.depend_rule(rule, oid, refs);
        Ok(())
    }

    /// The dependencies of the `_RETURN` rule of a view, as `InsertRule` and `recordDependencyOnExpr` record them. The rule is part of the view, and it depends on each object that the query reads. The objects come in the order of `eliminate_duplicate_dependencies`, from the highest OID down, and a column takes the place of its whole relation.
    fn depend_rule(&mut self, rule: u32, view: u32, refs: &[ObjRef]) {
        let object = ObjRef::new(PG_REWRITE, rule);
        self.depend(object, ObjRef::new(PG_CLASS, view), DepKind::Internal);
        let mut refs = refs.to_vec();
        refs.sort_by(|a, b| {
            b.oid
                .cmp(&a.oid)
                .then(a.class.cmp(&b.class))
                .then(a.sub.cast_unsigned().cmp(&b.sub.cast_unsigned()))
        });
        let mut kept: Vec<ObjRef> = Vec::with_capacity(refs.len());
        for r in refs {
            match kept.last_mut() {
                Some(prior) if prior.class == r.class && prior.oid == r.oid => {
                    if prior.sub == 0 {
                        prior.sub = r.sub;
                    } else if prior.sub != r.sub {
                        kept.push(r);
                    }
                }
                _ => kept.push(r),
            }
        }
        for referenced in kept {
            self.depend(object, referenced, DepKind::Normal);
        }
    }

    /// Sets `reloptions` of a relation.
    pub fn set_options(&mut self, oid: u32, options: Vec<String>) -> Result<()> {
        self.relation_mut(oid)?.options = options;
        Ok(())
    }

    /// Sets `nspacl` of a schema.
    pub fn set_schema_acl(&mut self, oid: u32, acl: Option<Vec<String>>) -> Result<()> {
        self.schemas.get_mut(&oid).ok_or_else(|| no_object("schema", oid))?.acl = acl;
        Ok(())
    }

    /// `DefineDomain`: the domain gets the OID after the OID of its array type, as `AssignTypeArrayOid` takes the first one. `refs` are the objects that the default refers to. The caller checks the base type, and adds the check constraints with [`Catalog::add_domain_check`] after this.
    pub fn create_domain(&mut self, new: NewDomain, refs: &[ObjRef]) -> Result<u32> {
        self.check_new_type(new.namespace, &new.name)?;
        let array = self.new_oid();
        let oid = self.new_oid();
        let array_name = self.array_type_name(&new.name, new.namespace);
        let base = new.domain.base;
        let collation = new.domain.collation;
        self.types.insert(
            oid,
            Type {
                oid,
                name: new.name,
                namespace: new.namespace,
                owner: new.owner,
                relation: 0,
                element: 0,
                array,
                domain: Some(new.domain),
            },
        );
        self.types.insert(
            array,
            Type {
                oid: array,
                name: array_name,
                namespace: new.namespace,
                owner: new.owner,
                relation: 0,
                element: oid,
                array: 0,
                domain: None,
            },
        );
        // `record_object_address_dependencies` sorts the objects by the OID of their catalog.
        let me = ObjRef::new(PG_TYPE, oid);
        self.depend(me, ObjRef::new(PG_TYPE, base), DepKind::Normal);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        if collation != 0 {
            self.depend(me, ObjRef::new(PG_COLLATION, collation), DepKind::Normal);
        }
        for &referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
        self.depend(ObjRef::new(PG_TYPE, array), me, DepKind::Internal);
        Ok(oid)
    }

    /// `ProcedureCreate`: a new function, which gets the next OID. It depends on its schema and on the objects in `refs`, which are the objects that its body refers to.
    ///
    /// # Errors
    ///
    /// `42723` when the schema has a function with the same name and the same argument types.
    pub fn create_function(&mut self, mut new: Function, refs: &[ObjRef]) -> Result<u32> {
        if self.functions.values().any(|f| {
            f.namespace == new.namespace && f.name == new.name && f.argtypes == new.argtypes
        }) {
            return Err(Error::new(
                SqlState::DUPLICATE_FUNCTION,
                format!("function \"{}\" already exists with same argument types", new.name),
            ));
        }
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_PROC, oid);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        for &referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
        self.functions.insert(oid, new);
        Ok(oid)
    }

    /// `domainAddCheckConstraint`: a check constraint of a domain. Without a name, the name is `domain_check`. `refs` are the objects that the expression refers to.
    pub fn add_domain_check(
        &mut self,
        domain: u32,
        name: Option<&str>,
        expr: String,
        refs: &[ObjRef],
    ) -> Result<u32> {
        let ty = self.types.get(&domain).ok_or_else(|| no_object("type", domain))?;
        let (domain_name, namespace) = (ty.name.clone(), ty.namespace);
        let name = match name {
            Some(name) => {
                if self.constraints_of_domain(domain).any(|c| c.name == name) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{name}\" for domain \"{domain_name}\" already exists"
                        ),
                    ));
                }
                name.to_string()
            }
            None => self.choose_constraint_name(&domain_name, None, "check", namespace),
        };
        let mut constraint = self.new_constraint(name, namespace, ConKind::Check, 0, Vec::new());
        constraint.domain = domain;
        constraint.expr = Some(expr);
        let me = ObjRef::new(PG_CONSTRAINT, constraint.oid);
        let oid = constraint.oid;
        self.insert_constraint(constraint);
        self.depend(me, ObjRef::new(PG_TYPE, domain), DepKind::Auto);
        for &referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
        Ok(oid)
    }

    /// Sets `relacl` of a relation.
    pub fn set_acl(&mut self, oid: u32, acl: Option<Vec<String>>) -> Result<()> {
        self.relation_mut(oid)?.acl = acl;
        Ok(())
    }

    /// `DefineSequence`. A sequence has no row type, and each column of a sequence is not null.
    pub fn create_sequence(&mut self, mut new: NewRelation, info: SequenceInfo) -> Result<u32> {
        self.check_new_relation(new.namespace, &new.name, false)?;
        for column in &mut new.columns {
            column.not_null = true;
        }
        let oid = self.new_oid();
        self.relations.insert(
            oid,
            Relation {
                oid,
                name: new.name,
                namespace: new.namespace,
                kind: RelKind::Sequence,
                owner: new.owner,
                row_type: 0,
                columns: new.columns,
                has_index: false,
                has_triggers: false,
                toast: 0,
                index: None,
                sequence: Some(info),
                view: None,
                options: Vec::new(),
                acl: None,
            },
        );
        self.depend(
            ObjRef::new(PG_CLASS, oid),
            ObjRef::new(PG_NAMESPACE, new.namespace),
            DepKind::Normal,
        );
        Ok(oid)
    }

    /// `OWNED BY`: the sequence goes with the column of the table.
    pub fn set_sequence_owner(&mut self, sequence: u32, table: u32, column: i16) {
        self.depend(ObjRef::new(PG_CLASS, sequence), ObjRef::column(table, column), DepKind::Auto);
    }

    fn relation_mut(&mut self, oid: u32) -> Result<&mut Relation> {
        self.relations.get_mut(&oid).ok_or_else(|| no_object("relation", oid))
    }

    fn column_mut(&mut self, relation: u32, column: i16) -> Result<&mut Column> {
        let index = usize::try_from(column).ok().and_then(|n| n.checked_sub(1));
        let rel = self.relation_mut(relation)?;
        index
            .and_then(|i| rel.columns.get_mut(i))
            .ok_or_else(|| no_object(&format!("attribute {column} of relation"), relation))
    }

    /// `StoreAttrDefault`: the default of a column. `refs` are the objects that the expression refers to.
    pub fn add_default(
        &mut self,
        relation: u32,
        column: i16,
        expr: String,
        refs: &[ObjRef],
    ) -> Result<u32> {
        self.column_mut(relation, column)?.has_default = true;
        let oid = self.new_oid();
        self.defaults.insert(oid, AttrDefault { oid, relation, column, expr });
        let me = ObjRef::new(PG_ATTRDEF, oid);
        self.depend(me, ObjRef::column(relation, column), DepKind::Auto);
        self.depend_on_expr(me, refs, relation, DepKind::Normal);
        Ok(oid)
    }

    fn relation_facts(&self, relation: u32) -> Result<(String, u32)> {
        let rel = self.relations.get(&relation).ok_or_else(|| no_object("relation", relation))?;
        Ok((rel.name.clone(), rel.namespace))
    }

    fn check_constraint_name(&self, relation: u32, name: &str, relname: &str) -> Result<()> {
        if self.constraints_of(relation).any(|c| c.name == name) {
            return Err(duplicate_constraint(name, relname));
        }
        Ok(())
    }

    fn insert_constraint(&mut self, constraint: Constraint) {
        let me = ObjRef::new(PG_CONSTRAINT, constraint.oid);
        let relation = constraint.relation;
        let keys = constraint.keys.clone();
        self.constraints.insert(constraint.oid, constraint);
        for key in keys {
            self.depend(me, ObjRef::column(relation, key), DepKind::Auto);
        }
    }

    fn new_constraint(
        &mut self,
        name: String,
        namespace: u32,
        kind: ConKind,
        relation: u32,
        keys: Vec<i16>,
    ) -> Constraint {
        Constraint {
            oid: self.new_oid(),
            name,
            namespace,
            kind,
            relation,
            domain: 0,
            keys,
            index: 0,
            deferrable: false,
            deferred: false,
            validated: true,
            no_inherit: false,
            expr: None,
            foreign: None,
        }
    }

    /// `StoreRelCheck`: a check constraint. `keys` are the columns that the expression refers to, in the order of their first use, and `refs` are the objects that the expression refers to.
    ///
    /// Without a name, the name is `table_column_check` when the expression refers to one column, else `table_check`.
    pub fn add_check(
        &mut self,
        relation: u32,
        name: Option<&str>,
        expr: String,
        keys: Vec<i16>,
        refs: &[ObjRef],
        no_inherit: bool,
    ) -> Result<u32> {
        let (relname, namespace) = self.relation_facts(relation)?;
        let name = match name {
            Some(name) => {
                self.check_constraint_name(relation, name, &relname)?;
                name.to_string()
            }
            None => {
                let column = match keys.as_slice() {
                    [key] => self
                        .relations
                        .get(&relation)
                        .and_then(|r| r.column(*key))
                        .map(|c| c.name.clone()),
                    _ => None,
                };
                self.choose_constraint_name(&relname, column.as_deref(), "check", namespace)
            }
        };
        let mut constraint = self.new_constraint(name, namespace, ConKind::Check, relation, keys);
        constraint.expr = Some(expr);
        constraint.no_inherit = no_inherit;
        let me = ObjRef::new(PG_CONSTRAINT, constraint.oid);
        let oid = constraint.oid;
        self.insert_constraint(constraint);
        self.depend_on_expr(me, refs, relation, DepKind::Normal);
        Ok(oid)
    }

    /// `StoreRelNotNull`: the not-null constraint of a column. Without a name, the name is `table_column_not_null`.
    pub fn add_not_null(
        &mut self,
        relation: u32,
        name: Option<&str>,
        column: i16,
        no_inherit: bool,
    ) -> Result<u32> {
        let (relname, namespace) = self.relation_facts(relation)?;
        let colname = self.column_mut(relation, column)?.name.clone();
        let name = match name {
            Some(name) => {
                self.check_constraint_name(relation, name, &relname)?;
                name.to_string()
            }
            None => self.choose_constraint_name(&relname, Some(&colname), "not_null", namespace),
        };
        self.column_mut(relation, column)?.not_null = true;
        let mut constraint =
            self.new_constraint(name, namespace, ConKind::NotNull, relation, vec![column]);
        constraint.no_inherit = no_inherit;
        let oid = constraint.oid;
        self.insert_constraint(constraint);
        Ok(oid)
    }

    /// `index_create`, and `index_constraint_create` for the index of a primary key or a unique constraint. The result is the OID of the index and the OID of the constraint.
    ///
    /// Without a name, the name is `table_pkey` for a primary key, `table_columns_key` for a unique constraint, else `table_columns_idx`.
    pub fn create_index(&mut self, new: NewIndex) -> Result<(u32, Option<u32>)> {
        let (relname, namespace) = self.relation_facts(new.table)?;
        let owner = self.relations.get(&new.table).map_or(0, |r| r.owner);
        let column_names = index_column_names(new.columns.iter().map(|c| c.name.as_str()));
        let name = match &new.name {
            Some(name) => name.clone(),
            None => {
                let addition = name_addition(column_names.iter().map(String::as_str));
                match new.constraint {
                    Some(ConKind::Primary) => {
                        self.choose_relation_name(&relname, None, "pkey", namespace, true)
                    }
                    Some(_) => {
                        self.choose_relation_name(&relname, Some(&addition), "key", namespace, true)
                    }
                    None => self.choose_relation_name(
                        &relname,
                        Some(&addition),
                        "idx",
                        namespace,
                        false,
                    ),
                }
            }
        };
        self.check_new_relation(namespace, &name, false)?;
        if new.constraint.is_some() {
            self.check_constraint_name(new.table, &name, &relname)?;
        }
        let oid = self.new_oid();
        let keys: Vec<i16> = new.columns.iter().map(|c| c.key).collect();
        let key_columns = &new.columns[..new.key_count.min(new.columns.len())];
        let info = IndexInfo {
            table: new.table,
            keys: keys.clone(),
            key_count: i16::try_from(new.key_count).unwrap_or(i16::MAX),
            unique: new.unique,
            nulls_not_distinct: new.nulls_not_distinct,
            primary: new.constraint == Some(ConKind::Primary),
            immediate: !new.deferrable,
            method: new.method,
            collations: key_columns.iter().map(|c| c.collation).collect(),
            classes: key_columns.iter().map(|c| c.class).collect(),
            options: key_columns.iter().map(|c| c.option).collect(),
            exprs: new.exprs,
            predicate: new.predicate,
        };
        let table_columns = self.relations.get(&new.table).map_or(&[][..], |r| &r.columns[..]);
        let columns = new
            .columns
            .iter()
            .zip(column_names)
            .map(|(c, name)| {
                let mut column = Column::new(name, c.ty, c.typmod, c.collation);
                let from = usize::try_from(c.key).ok().and_then(|k| k.checked_sub(1));
                if let Some(from) = from.and_then(|k| table_columns.get(k)) {
                    column.ndims = from.ndims;
                }
                column
            })
            .collect();
        self.relations.insert(
            oid,
            Relation {
                oid,
                name: name.clone(),
                namespace,
                kind: RelKind::Index,
                owner,
                row_type: 0,
                columns,
                has_index: false,
                has_triggers: false,
                toast: 0,
                index: Some(info),
                sequence: None,
                view: None,
                options: Vec::new(),
                acl: None,
            },
        );
        let me = ObjRef::new(PG_CLASS, oid);
        let mut constraint_oid = None;
        if let Some(kind) = new.constraint {
            let mut constraint = self.new_constraint(name, namespace, kind, new.table, keys);
            constraint.index = oid;
            constraint.no_inherit = true;
            constraint.deferrable = new.deferrable;
            constraint.deferred = new.deferred;
            let referenced = ObjRef::new(PG_CONSTRAINT, constraint.oid);
            constraint_oid = Some(constraint.oid);
            self.insert_constraint(constraint);
            self.depend(me, referenced, DepKind::Internal);
        } else {
            let simple: Vec<i16> = keys.iter().copied().filter(|&k| k != 0).collect();
            if simple.is_empty() {
                self.depend(me, ObjRef::new(PG_CLASS, new.table), DepKind::Auto);
            }
            for key in simple {
                self.depend(me, ObjRef::column(new.table, key), DepKind::Auto);
            }
        }
        self.depend_on_expr(me, &new.refs, new.table, DepKind::Auto);
        self.relation_mut(new.table)?.has_index = true;
        Ok((oid, constraint_oid))
    }

    /// `ATAddForeignKeyConstraint`: a foreign key. The four triggers of PostgreSQL use the next four OIDs. Without a name, the name is `table_columns_fkey`.
    pub fn add_foreign_key(&mut self, new: NewForeignKey) -> Result<u32> {
        let (relname, namespace) = self.relation_facts(new.table)?;
        let name = match &new.name {
            Some(name) => {
                self.check_constraint_name(new.table, name, &relname)?;
                name.clone()
            }
            None => {
                let rel = self
                    .relations
                    .get(&new.table)
                    .ok_or_else(|| no_object("relation", new.table))?;
                let columns: Vec<&str> = new
                    .keys
                    .iter()
                    .filter_map(|&k| rel.column(k))
                    .map(|c| c.name.as_str())
                    .collect();
                let addition = name_addition(columns);
                self.choose_constraint_name(&relname, Some(&addition), "fkey", namespace)
            }
        };
        let mut constraint =
            self.new_constraint(name, namespace, ConKind::Foreign, new.table, new.keys);
        constraint.index = new.index;
        constraint.no_inherit = true;
        constraint.deferrable = new.deferrable;
        constraint.deferred = new.deferred;
        let oid = constraint.oid;
        let referenced = new.foreign.table;
        let referenced_keys = new.foreign.keys.clone();
        constraint.foreign = Some(new.foreign);
        self.insert_constraint(constraint);
        let me = ObjRef::new(PG_CONSTRAINT, oid);
        for key in referenced_keys {
            self.depend(me, ObjRef::column(referenced, key), DepKind::Normal);
        }
        self.depend(me, ObjRef::new(PG_CLASS, new.index), DepKind::Normal);
        self.skip_oids(FOREIGN_KEY_TRIGGERS);
        self.relation_mut(new.table)?.has_triggers = true;
        self.relation_mut(referenced)?.has_triggers = true;
        Ok(oid)
    }

    /// `create_toast_table`: the TOAST table of a table and its index, in the schema `pg_toast`. The TOAST table gets an OID, then its index. They have no row type.
    pub fn create_toast(&mut self, table: u32) -> Result<u32> {
        let owner = self.relations.get(&table).ok_or_else(|| no_object("relation", table))?.owner;
        let oid = self.new_oid();
        let index = self.new_oid();
        let name = format!("pg_toast_{table}");
        let chunk_id = Column::new("chunk_id", OID, -1, 0);
        let chunk_seq = Column::new("chunk_seq", INT4, -1, 0);
        let chunk_data = Column::new("chunk_data", BYTEA, -1, 0);
        self.relations.insert(
            oid,
            Relation {
                oid,
                name: name.clone(),
                namespace: PG_TOAST_NAMESPACE,
                kind: RelKind::Toast,
                owner,
                row_type: 0,
                columns: vec![chunk_id.clone(), chunk_seq.clone(), chunk_data],
                has_index: true,
                has_triggers: false,
                toast: 0,
                index: None,
                sequence: None,
                view: None,
                options: Vec::new(),
                acl: None,
            },
        );
        let info = IndexInfo {
            table: oid,
            keys: vec![1, 2],
            key_count: 2,
            unique: true,
            nulls_not_distinct: false,
            primary: true,
            immediate: true,
            method: BTREE,
            collations: vec![0, 0],
            classes: vec![OID_OPS, INT4_OPS],
            options: vec![0, 0],
            exprs: None,
            predicate: None,
        };
        self.relations.insert(
            index,
            Relation {
                oid: index,
                name: format!("{name}_index"),
                namespace: PG_TOAST_NAMESPACE,
                kind: RelKind::Index,
                owner,
                row_type: 0,
                columns: vec![chunk_id, chunk_seq],
                has_index: false,
                has_triggers: false,
                toast: 0,
                index: Some(info),
                sequence: None,
                view: None,
                options: Vec::new(),
                acl: None,
            },
        );
        self.relation_mut(table)?.toast = oid;
        self.depend(ObjRef::new(PG_CLASS, oid), ObjRef::new(PG_CLASS, table), DepKind::Internal);
        for column in 1..=2 {
            self.depend(ObjRef::new(PG_CLASS, index), ObjRef::column(oid, column), DepKind::Auto);
        }
        Ok(oid)
    }
}

#[cfg(test)]
mod tests;

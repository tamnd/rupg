//! The rupg catalog of user objects: schemas, tables, columns, constraints, indexes, roles, privileges.
//!
//! At M2 the catalog holds the schemas, tables, sequences, indexes, views, column defaults, constraints and text search objects that DDL makes. Each object gets its OID in the order that PostgreSQL 19 gives OIDs, and each object without a name in the statement gets the name that PostgreSQL gives it. Thus after the same statements on a new cluster, the OIDs and the names are the same as in PostgreSQL.
//!
//! The catalog does not read SQL. The analyzer turns a statement into calls of this crate in the order of `DefineRelation`, `DefineIndex` and `ATAddForeignKeyConstraint`, and it gives each expression as text that only the analyzer reads back. The catalog makes the TOAST tables that PostgreSQL makes, with their indexes, but they hold no data. It also makes the four internal triggers of each foreign key, which have no work to do because the tables have no rows. The catalog also records the rows of `pg_depend` in the order that PostgreSQL inserts them.
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.

#![forbid(unsafe_code)]

pub mod database;
pub mod names;
pub mod size;
pub mod toast;

mod alter_type;
mod drop;
mod enum_label;
mod operator;
mod owner;
mod sequence;
mod set_schema;
mod tsearch;

pub use drop::{Refusal, Target, flag};
pub use operator::{Operator, PG_OPERATOR};
pub use owner::acl_new_owner;
pub use sequence::SequenceData;
pub use tsearch::{
    PG_TS_CONFIG, PG_TS_DICT, PG_TS_PARSER, PG_TS_TEMPLATE, TsConfig, TsDictionary, TsMapping,
    TsParser, TsTemplate,
};

use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_common::{Error, Object, Result, SqlState};

pub use database::Database;
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
/// The OID of the catalog `pg_cast`.
pub const PG_CAST: u32 = 2605;
/// The OID of the catalog `pg_opclass`.
pub const PG_OPCLASS: u32 = 2616;
/// The OID of the type `text`.
const TEXT: u32 = 25;
/// The OID of the language `internal`.
const INTERNAL_LANGUAGE: u32 = 12;
/// `BOOTSTRAP_SUPERUSERID`, the owner of the built-in objects and of the constructors of a range type.
pub const BOOTSTRAP_SUPERUSER: u32 = 10;
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
/// The OID of the catalog `pg_language`.
pub const PG_LANGUAGE: u32 = 2612;
/// The OID of the catalog `pg_extension`.
pub const PG_EXTENSION: u32 = 3079;
/// The OID of the `btree` access method.
pub const BTREE: u32 = 403;
/// The OID of the catalog `pg_trigger`.
pub const PG_TRIGGER: u32 = 2620;
/// The OID of `RI_FKey_check_ins`. The other functions of the foreign keys follow it in the order of `ri_triggers.c`.
const RI_FKEY_CHECK_INS: u32 = 1644;
/// The OID of `unique_key_recheck`, the function of the trigger of a deferrable unique constraint.
const UNIQUE_KEY_RECHECK: u32 = 1250;
/// `TRIGGER_TYPE_ROW`.
pub const TRIGGER_ROW: i16 = 1;
/// `TRIGGER_TYPE_BEFORE`. A trigger with neither this bit nor [`TRIGGER_INSTEAD`] fires after the event.
pub const TRIGGER_BEFORE: i16 = 2;
/// `TRIGGER_TYPE_INSERT`.
pub const TRIGGER_INSERT: i16 = 4;
/// `TRIGGER_TYPE_DELETE`.
pub const TRIGGER_DELETE: i16 = 8;
/// `TRIGGER_TYPE_UPDATE`.
pub const TRIGGER_UPDATE: i16 = 16;
/// `TRIGGER_TYPE_TRUNCATE`.
pub const TRIGGER_TRUNCATE: i16 = 32;
/// `TRIGGER_TYPE_INSTEAD`.
pub const TRIGGER_INSTEAD: i16 = 64;

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
    /// The relation of a composite type that `CREATE TYPE ... AS` makes, `c`.
    Composite,
    /// A partitioned table, `p`. It has no storage. Its rows are in its partitions.
    Partitioned,
    /// The index of a partitioned table, `I`. It has no storage. Its partitions are the indexes of the partitions of the table.
    PartitionedIndex,
    /// A materialized view, `m`. It keeps the rows of its query in a heap as a table does, and it has a `_RETURN` rule as a view does.
    MatView,
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
            RelKind::Composite => 'c',
            RelKind::Partitioned => 'p',
            RelKind::PartitionedIndex => 'I',
            RelKind::MatView => 'm',
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
    /// `attidentity`: `a` for `GENERATED ALWAYS`, `d` for `GENERATED BY DEFAULT`, or 0 for a column that is not an identity column.
    pub identity: u8,
    /// `attisdropped`. A dropped column keeps its place, so the later columns keep their numbers, but it has no name that a query can use and no type.
    pub dropped: bool,
    /// `attstattarget`: the statistics target of `SET STATISTICS`, or `None` for the default.
    pub stat_target: Option<i16>,
    /// `attstorage` of `STORAGE` or `SET STORAGE`, or 0 for the storage of the type.
    pub storage: u8,
    /// `attcompression`: `p` for `pglz`, `l` for `lz4`, or 0 for the default method.
    pub compression: u8,
    /// `attoptions`: each option as `name=value`.
    pub options: Vec<String>,
    /// `attislocal`: false for a column that only a parent table gives.
    pub local: bool,
    /// `attinhcount`: the number of parent tables that give the column.
    pub inhcount: i16,
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
            identity: 0,
            dropped: false,
            stat_target: None,
            storage: 0,
            compression: 0,
            options: Vec::new(),
            local: true,
            inhcount: 0,
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
    /// `relpages` of the index: the pages of the main fork when the index is new.
    pub pages: i32,
    /// `indisclustered`: `CLUSTER ON` chose the index.
    pub clustered: bool,
    /// `indisreplident`: `REPLICA IDENTITY USING INDEX` chose the index.
    pub replica_identity: bool,
    /// `indisvalid`: false for an index of a partitioned table that a partition does not have yet.
    pub valid: bool,
    /// The index of the parent table that the index is a partition of, from `pg_inherits`, or 0.
    pub parent: u32,
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
    /// The file of the sequence, `relfilenode`. [`Catalog::create_sequence`] sets it to the OID of the sequence, and a rewrite of `ALTER SEQUENCE` gives a new file.
    pub file: u32,
    /// The row of the file when the statement made the file. `nextval` and `setval` change the row outside of the transactions, so the session keeps the row of a file after the first change.
    pub data: SequenceData,
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
    /// `relreplident`: `d`, `n`, `f` or `i`, or 0 for the default of the kind of the relation.
    pub replica_identity: u8,
    /// True when `relpersistence` is `u`. The rows of an unlogged table are kept as the rows of the other tables. A temporary relation has `t` because of its schema.
    pub unlogged: bool,
    /// The partition key of a partitioned table, one row of `pg_partitioned_table`.
    pub partition_key: Option<PartitionKey>,
    /// The parent table and the bound of a partition.
    pub partition_of: Option<PartitionOf>,
    /// The parent tables of `INHERITS`: the parent at index `i` has `inhseqno` `i + 1`. `NO INHERIT` leaves 0 in the place of the parent, so the other parents keep their numbers. A partition has its parent in [`Relation::partition_of`].
    pub inherits: Vec<u32>,
    /// `reloftype`: the composite type of a typed table, or 0.
    pub of_type: u32,
}

/// The strategy of a partition key, `partstrat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartStrategy {
    /// `HASH`, `h`.
    Hash,
    /// `LIST`, `l`.
    List,
    /// `RANGE`, `r`.
    Range,
}

impl PartStrategy {
    /// The `partstrat` letter.
    pub fn code(self) -> char {
        match self {
            PartStrategy::Hash => 'h',
            PartStrategy::List => 'l',
            PartStrategy::Range => 'r',
        }
    }
}

/// The partition key of a partitioned table, one row of `pg_partitioned_table`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionKey {
    /// The strategy.
    pub strategy: PartStrategy,
    /// `partattrs`: the column number of each key column, or 0 for an expression.
    pub attrs: Vec<i16>,
    /// `partclass`: the operator class of each key column.
    pub classes: Vec<u32>,
    /// `partcollation`: the collation of each key column, or 0.
    pub collations: Vec<u32>,
    /// `partexprs`: the expressions of the key columns with number 0, in the form of the analyzer.
    pub exprs: Option<String>,
}

/// A datum of the bound of a range partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeDatum {
    /// `MINVALUE`.
    MinValue,
    /// `MAXVALUE`.
    MaxValue,
    /// A constant, in the form of the analyzer.
    Value(String),
}

/// The bound of a partition, `relpartbound`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartitionBound {
    /// `DEFAULT`.
    Default,
    /// `FOR VALUES WITH (MODULUS m, REMAINDER r)`.
    Hash {
        /// The modulus.
        modulus: i32,
        /// The remainder.
        remainder: i32,
    },
    /// `FOR VALUES IN (...)`: each constant in the form of the analyzer, or `None` for `NULL`.
    List(Vec<Option<String>>),
    /// `FOR VALUES FROM (...) TO (...)`.
    Range {
        /// The lower bound, one datum for each key column.
        lower: Vec<RangeDatum>,
        /// The upper bound, one datum for each key column.
        upper: Vec<RangeDatum>,
    },
}

/// The parent table and the bound of a partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionOf {
    /// The OID of the parent table, `inhparent`.
    pub parent: u32,
    /// The bound.
    pub bound: PartitionBound,
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
    /// `relispopulated`: false for a materialized view of `WITH NO DATA` until a `REFRESH` fills it. A view is always populated.
    pub populated: bool,
}

impl Relation {
    /// The number of the column with the name, from 1.
    pub fn column_number(&self, name: &str) -> Option<i16> {
        let index = self.columns.iter().position(|c| !c.dropped && c.name == name)?;
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
    /// The labels of an enum type in their sort order, as `pg_enum` holds them, or `None` for a type of another kind.
    pub labels: Option<Vec<EnumLabel>>,
    /// The facts of a range type or of a multirange type, or `None` for a type of another kind.
    pub range: Option<RangePart>,
}

/// The facts of a range type, one row of `pg_range`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeInfo {
    /// `rngsubtype`: the OID of the subtype.
    pub subtype: u32,
    /// `rngcollation`: the collation of the subtype, or 0 for a subtype that is not collatable.
    pub collation: u32,
    /// `rngsubopc`: the OID of the `btree` operator class of the subtype.
    pub subopc: u32,
    /// `rngcanonical`: the OID of the canonical function, or 0.
    pub canonical: u32,
    /// `rngsubdiff`: the OID of the difference function of the subtype, or 0.
    pub subdiff: u32,
    /// `rngmultitypid`: the OID of the multirange type. [`Catalog::create_range`] sets it.
    pub multirange: u32,
    /// `typalign` of the range type, of its multirange type and of their array types: `d` when the subtype has the alignment `d`, else `i`.
    pub align: u8,
    /// The OIDs of the constructors, in the order of their OIDs: two of the range type, then three of the multirange type. [`Catalog::create_range`] sets them.
    pub constructors: [u32; 5],
}

/// The part of a type in a range type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangePart {
    /// A range type, with its row of `pg_range`.
    Range(RangeInfo),
    /// The multirange type of the range type with the OID.
    Multirange(u32),
}

/// A cast, one row of `pg_cast`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cast {
    /// The OID.
    pub oid: u32,
    /// `castsource`.
    pub source: u32,
    /// `casttarget`.
    pub target: u32,
    /// `castfunc`: the OID of the function, or 0.
    pub func: u32,
    /// `castcontext`: `e` explicit, `a` assignment or `i` implicit.
    pub context: u8,
    /// `castmethod`: `f` function, `i` input and output, or `b` binary.
    pub method: u8,
}

/// A label of an enum type, one row of `pg_enum`.
#[derive(Clone, Debug)]
pub struct EnumLabel {
    /// The OID of the label. The labels of `CREATE TYPE ... AS ENUM` have even OIDs in their sort order.
    pub oid: u32,
    /// `enumsortorder`.
    pub sort_order: f32,
    /// `enumlabel`.
    pub label: String,
}

impl PartialEq for EnumLabel {
    fn eq(&self, other: &EnumLabel) -> bool {
        self.oid == other.oid
            && self.sort_order.to_bits() == other.sort_order.to_bits()
            && self.label == other.label
    }
}

impl Eq for EnumLabel {}

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
    /// `typnotnull`: the domain has a not-null constraint.
    pub not_null: bool,
}

/// A function, one row of `pg_proc`: a function of the setup of a new cluster, such as a function in SQL of `information_schema`, or a function that `CREATE FUNCTION` makes.
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
    /// `prokind`: `f` for a function or `p` for a procedure.
    pub kind: u8,
    /// `proisstrict`: true when a null argument gives null without a call.
    pub strict: bool,
    /// `proleakproof`: true when the function has no side effects and gives no information about its arguments other than its result.
    pub leakproof: bool,
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
    /// `proargdefaults`: the defaults of the last input arguments, in the stored form of a list of expressions, or `None` when no argument has a default.
    pub argdefaults: Option<String>,
    /// `pronargdefaults`: the number of the defaults in `argdefaults`.
    pub nargdefaults: i16,
    /// `prosrc`: the body in a string, or empty for a body in SQL.
    pub src: String,
    /// `probin`: the file of a function in C, or empty for another function.
    pub bin: String,
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
    /// The constraint of a constraint trigger, `t`.
    Trigger,
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
            ConKind::Trigger => 't',
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
    /// `conislocal`: false for a constraint that only a parent table gives.
    pub local: bool,
    /// `coninhcount`: the number of parent tables that give the constraint.
    pub inhcount: i16,
    /// `conparentid`: the constraint of the parent table that a constraint of a partition comes from, or 0.
    pub parent: u32,
    /// The expression of a check constraint, in the form of the analyzer.
    pub expr: Option<String>,
    /// The facts of a foreign key.
    pub foreign: Option<ForeignKey>,
}

/// A trigger, one row of `pg_trigger`: a trigger of `CREATE TRIGGER`, or an internal trigger of a foreign key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trigger {
    /// The OID.
    pub oid: u32,
    /// The OID of the table of the trigger.
    pub relation: u32,
    /// The name.
    pub name: String,
    /// The OID of the function.
    pub function: u32,
    /// `tgtype`: the bits of the level, the time and the events.
    pub kind: i16,
    /// `tgconstrrelid`: the other table of the foreign key.
    pub constraint_relation: u32,
    /// `tgconstrindid`: the unique index of the referenced table.
    pub constraint_index: u32,
    /// `tgconstraint`: the OID of the foreign key.
    pub constraint: u32,
    /// `tgdeferrable`.
    pub deferrable: bool,
    /// `tginitdeferred`.
    pub deferred: bool,
    /// `tgenabled`: `O` when the trigger fires in the origin mode, `D` when it is disabled, `R` when it fires only in the replica mode, and `A` when it always fires.
    pub enabled: u8,
    /// `tgisinternal`: true for a trigger of a foreign key.
    pub internal: bool,
    /// `tgargs`: the arguments of the function, as text.
    pub args: Vec<String>,
    /// `tgattr`: the numbers of the columns of `UPDATE OF`, in the order of the statement.
    pub columns: Vec<i16>,
    /// `tgoldtable`: the name of the transition table of `REFERENCING OLD TABLE`.
    pub old_table: Option<String>,
    /// `tgnewtable`: the name of the transition table of `REFERENCING NEW TABLE`.
    pub new_table: Option<String>,
    /// `tgqual`: the stored form of the condition of `WHEN`, in which the relation 0 is `old` and the relation 1 is `new`.
    pub qual: Option<String>,
    /// `tgparentid`: the trigger of the parent table that the trigger of a partition is a clone of, or 0.
    pub parent: u32,
}

impl Trigger {
    /// True when the trigger fires with `session_replication_role` at `origin`, the default: `tgenabled` is `O` or `A`.
    pub fn fires(&self) -> bool {
        matches!(self.enabled, b'O' | b'A')
    }
}

/// The facts of a new trigger for [`Catalog::create_trigger`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewTrigger {
    /// The name.
    pub name: String,
    /// The OID of the relation.
    pub relation: u32,
    /// The OID of the function.
    pub function: u32,
    /// `tgtype`.
    pub kind: i16,
    /// `tgargs`.
    pub args: Vec<String>,
    /// `tgattr`.
    pub columns: Vec<i16>,
    /// `tgoldtable`.
    pub old_table: Option<String>,
    /// `tgnewtable`.
    pub new_table: Option<String>,
    /// `tgqual`.
    pub qual: Option<String>,
    /// True for a trigger of `CREATE CONSTRAINT TRIGGER`, which gets a row of `pg_constraint`.
    pub constraint: bool,
    /// `tgconstrrelid`: the table of `FROM` of a constraint trigger, else 0.
    pub constraint_relation: u32,
    /// `tgdeferrable`.
    pub deferrable: bool,
    /// `tginitdeferred`.
    pub deferred: bool,
    /// `tgparentid`: the trigger of the parent table for a clone in a partition, else 0.
    pub parent: u32,
    /// `tgenabled` of a clone, which is the state of the trigger of the parent table, or 0 for `O`.
    pub enabled: u8,
}

/// A rule of `CREATE RULE`, one row of `pg_rewrite`. The `_RETURN` rule of a view is part of [`ViewInfo`], not a [`Rule`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the relation of the rule.
    pub relation: u32,
    /// `ev_type`: the event, from `1` for `SELECT` to `4` for `DELETE`.
    pub event: u8,
    /// `is_instead`: true for `DO INSTEAD`.
    pub instead: bool,
    /// `ev_enabled`: `O` when the rule fires in the origin mode.
    pub enabled: u8,
    /// True when the rule has a condition of `WHERE`.
    pub qualified: bool,
    /// The text of the statement that made the rule or that last replaced it. Only the analyzer reads it back.
    pub text: String,
    /// The schemas of `search_path` when that statement ran, in order.
    pub path: Vec<u32>,
}

/// The facts of a new rule for [`Catalog::create_rule`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRule {
    /// The name.
    pub name: String,
    /// The OID of the relation.
    pub relation: u32,
    /// `ev_type`.
    pub event: u8,
    /// `is_instead`.
    pub instead: bool,
    /// True when the rule has a condition of `WHERE`.
    pub qualified: bool,
    /// The text of the statement.
    pub text: String,
    /// The schemas of `search_path`.
    pub path: Vec<u32>,
}

/// A collation, one row of `pg_collation`. The catalog holds the collations that `pg_import_system_collations` makes when the cluster starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collation {
    /// The OID.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the schema.
    pub namespace: u32,
    /// The OID of the owner.
    pub owner: u32,
    /// `collprovider`: `c` for the C library and `i` for ICU.
    pub provider: u8,
    /// `collencoding`: the encoding, or -1 for any encoding.
    pub encoding: i32,
    /// `collcollate` and `collctype`: the locale of the C library, or `None` for ICU.
    pub libc_locale: Option<String>,
    /// `colllocale`: the locale of ICU, or `None` for the C library.
    pub locale: Option<String>,
    /// `collversion`, or `None`.
    pub version: Option<String>,
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
    /// `e`: the dependent object is a member of the referenced extension.
    Extension,
    /// `P`: the dependent object is a partition of the referenced object, as an index of a partition is of the index of the partitioned table.
    PartitionPri,
    /// `S`: the dependent object is a partition of an object of the referenced object, as an index of a partition is of the index of the partitioned table through the partition.
    PartitionSec,
}

impl DepKind {
    /// The `deptype` letter.
    pub fn code(self) -> char {
        match self {
            DepKind::Normal => 'n',
            DepKind::Auto => 'a',
            DepKind::Internal => 'i',
            DepKind::Extension => 'e',
            DepKind::PartitionPri => 'P',
            DepKind::PartitionSec => 'S',
        }
    }
}

/// A procedural language, one row of `pg_language`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Language {
    /// The OID. [`Catalog::create_language`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// `lanpltrusted`: true when a user without the superuser attribute can make functions in the language.
    pub trusted: bool,
    /// `lanplcallfoid`: the OID of the call handler.
    pub handler: u32,
    /// `laninline`: the OID of the handler of `DO`, or 0.
    pub inline: u32,
    /// `lanvalidator`: the OID of the validator, or 0.
    pub validator: u32,
}

/// An extension, one row of `pg_extension`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
    /// The OID. [`Catalog::create_extension`] sets it.
    pub oid: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// The OID of the schema of the objects of the extension.
    pub namespace: u32,
    /// `extrelocatable`: true when the objects can move to another schema.
    pub relocatable: bool,
    /// `extversion`: the version.
    pub version: String,
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

/// A new enum type for [`Catalog::create_enum`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewEnum {
    /// The OID of the schema.
    pub namespace: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// The labels in their sort order.
    pub labels: Vec<String>,
}

/// A new range type for [`Catalog::create_range`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRange {
    /// The OID of the schema.
    pub namespace: u32,
    /// The name.
    pub name: String,
    /// The OID of the owner role.
    pub owner: u32,
    /// The OID of the schema of the multirange type.
    pub multirange_namespace: u32,
    /// The name of the multirange type.
    pub multirange_name: String,
    /// The row of `pg_range`.
    pub info: RangeInfo,
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
    /// The pages of the main fork of the new index, from [`size::empty_index_pages`].
    pub pages: i32,
}

/// The side of a foreign key that a row of `pg_constraint` serves, as `addFkConstraint` takes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FkSide {
    /// The constraint of the statement, for both tables.
    Both,
    /// A clone for a partition of the referenced table.
    Referenced,
    /// A clone for a partition of the referencing table.
    Referencing,
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
    casts: BTreeMap<u32, Cast>,
    operators: BTreeMap<u32, Operator>,
    languages: BTreeMap<u32, Language>,
    extensions: BTreeMap<u32, Extension>,
    triggers: BTreeMap<u32, Trigger>,
    rules: BTreeMap<u32, Rule>,
    collations: Arc<BTreeMap<u32, Collation>>,
    ts_templates: BTreeMap<u32, TsTemplate>,
    ts_parsers: BTreeMap<u32, TsParser>,
    ts_dictionaries: BTreeMap<u32, TsDictionary>,
    ts_configs: BTreeMap<u32, TsConfig>,
    ts_mappings: Vec<TsMapping>,
    depends: Vec<Depend>,
    descriptions: Arc<BTreeMap<ObjRef, String>>,
    /// The ACLs that `GRANT` and `REVOKE` gave to the functions, the types, the languages and the columns, and to the objects of `initdb`. The relations and the schemas of the session keep their ACL in their own rows.
    acls: Arc<BTreeMap<ObjRef, Vec<String>>>,
    /// `uncommitted_enum_types`: the enum types that the open transaction made. [`Catalog::commit_enums`] clears them at the commit.
    uncommitted_types: Vec<u32>,
    /// `uncommitted_enum_values`: the labels that `ALTER TYPE ... ADD VALUE` of the open transaction gave to an enum type that an earlier transaction made. A statement cannot use such a label until the commit.
    uncommitted_labels: Vec<u32>,
}

impl Default for Catalog {
    fn default() -> Catalog {
        Catalog::new()
    }
}

/// The number of the encoding UTF8, the encoding of each database.
const UTF8: i32 = 6;

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

/// The place of a function of the foreign keys in the order of `ri_triggers.c`, from 0 for `RI_FKey_check_ins` to 11 for `RI_FKey_noaction_upd`, or `None` for another function. The functions of the referencing table are 0 and 1. For the referenced table, an even place is a function of `DELETE` and an odd place a function of `UPDATE`.
pub fn foreign_key_function(function: u32) -> Option<u32> {
    function.checked_sub(RI_FKEY_CHECK_INS).filter(|&at| at <= 11)
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

/// True when a new collation of the encoding `new` cannot have the name of a collation of the encoding `existing` in the same schema, as `CollationCreate` checks it. A collation of any encoding (-1) conflicts with every collation, and a collation of `UTF8` conflicts with a new collation of any encoding.
pub fn collation_conflict(existing: i32, new: i32) -> bool {
    existing == new || existing == -1 || (new == -1 && existing == UTF8)
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
            casts: BTreeMap::new(),
            operators: BTreeMap::new(),
            languages: BTreeMap::new(),
            extensions: BTreeMap::new(),
            triggers: BTreeMap::new(),
            rules: BTreeMap::new(),
            collations: Arc::default(),
            ts_templates: BTreeMap::new(),
            ts_parsers: BTreeMap::new(),
            ts_dictionaries: BTreeMap::new(),
            ts_configs: BTreeMap::new(),
            ts_mappings: Vec::new(),
            depends: Vec::new(),
            descriptions: Arc::default(),
            acls: Arc::default(),
            uncommitted_types: Vec::new(),
            uncommitted_labels: Vec::new(),
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

    /// Uses `count` OIDs that PostgreSQL uses and rupg does not, such as the OID of a table that a check refuses after the OID was given.
    pub fn skip_oids(&mut self, count: u32) {
        for _ in 0..count {
            self.new_oid();
        }
    }

    /// `GetNewObjectId` for an object that the catalog does not keep, such as a role: the next OID of the counter.
    pub fn take_oid(&mut self) -> u32 {
        self.new_oid()
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

    /// `isAnyTempNamespace`: true for a temporary schema `pg_temp_N` or its TOAST schema `pg_toast_temp_N` of any session. A user cannot make a schema with the prefix `pg_`, so the name tells.
    pub fn is_temp_schema(&self, oid: u32) -> bool {
        self.schemas.get(&oid).is_some_and(|s| {
            s.name
                .strip_prefix("pg_temp_")
                .or_else(|| s.name.strip_prefix("pg_toast_temp_"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
    }

    /// `GetTempToastNamespace`: the TOAST schema of the tables of a schema, which is `pg_toast_temp_N` for the temporary schema `pg_temp_N` and `pg_toast` for the other schemas.
    fn toast_namespace(&self, namespace: u32) -> u32 {
        self.schemas
            .get(&namespace)
            .and_then(|s| s.name.strip_prefix("pg_temp_"))
            .and_then(|n| self.schema_by_name(&format!("pg_toast_temp_{n}")))
            .map_or(PG_TOAST_NAMESPACE, |s| s.oid)
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

    /// The triggers in the order of their OIDs.
    pub fn triggers(&self) -> impl Iterator<Item = &Trigger> {
        self.triggers.values()
    }

    /// The trigger with the OID.
    pub fn trigger(&self, oid: u32) -> Option<&Trigger> {
        self.triggers.get(&oid)
    }

    /// The triggers of a relation in the order of their names, as the index `pg_trigger_tgrelid_tgname_index` gives them.
    pub fn triggers_of(&self, relation: u32) -> Vec<&Trigger> {
        let mut list: Vec<&Trigger> =
            self.triggers.values().filter(|t| t.relation == relation).collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// The trigger of the relation with the name.
    pub fn trigger_by_name(&self, relation: u32, name: &str) -> Option<&Trigger> {
        self.triggers.values().find(|t| t.relation == relation && t.name == name)
    }

    /// `CreateTriggerFiringOn`: a new trigger, or with `replace` new facts for the trigger of the relation with the same name. A replaced trigger keeps its OID and `tgenabled`, and gets new dependencies. The trigger depends on its function, then on its relation, then on each column of `UPDATE OF`, then on the objects that the condition of `WHEN` reads. A clone in a partition then has the partition dependencies on the trigger of the parent table and on its relation. The relation gets `relhastriggers`. The result is the OID of the trigger.
    ///
    /// A constraint trigger first gets its constraint of the kind `t`, with the name of the trigger, as `CreateConstraintEntry` makes it. The trigger then depends on the table of `FROM`, and the constraint depends on the trigger, so that `DROP TRIGGER` drops the constraint.
    ///
    /// # Errors
    ///
    /// `42710` when the relation has a trigger with the name and `replace` is false, or when that trigger is internal, or when it is a clone and the new trigger is not. `23505` when the relation has a constraint with the name of a new constraint trigger.
    pub fn create_trigger(
        &mut self,
        new: NewTrigger,
        replace: bool,
        qual: &[ObjRef],
    ) -> Result<u32> {
        let old = self
            .trigger_by_name(new.relation, &new.name)
            .map(|t| (t.oid, t.internal || (t.parent != 0 && new.parent == 0)));
        let constraint = if new.constraint && old.is_none() {
            if self.constraints_of(new.relation).any(|c| c.domain == 0 && c.name == new.name) {
                return Err(Error::new(
                    SqlState::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"",
                )
                .with_detail(format!(
                    "Key (conrelid, contypid, conname)=({}, 0, {}) already exists.",
                    new.relation, new.name
                ))
                .with_object(Object::Schema, "pg_catalog")
                .with_object(Object::Table, "pg_constraint")
                .with_object(Object::Constraint, "pg_constraint_conrelid_contypid_conname_index"));
            }
            let namespace = self.relation(new.relation).map_or(0, |r| r.namespace);
            let mut constraint = self.new_constraint(
                new.name.clone(),
                namespace,
                ConKind::Trigger,
                new.relation,
                Vec::new(),
            );
            constraint.deferrable = new.deferrable;
            constraint.deferred = new.deferred;
            constraint.no_inherit = true;
            Some(constraint)
        } else {
            None
        };
        let relation = self.relation(new.relation).map_or("", |r| r.name.as_str());
        let oid = match old {
            Some(_) if !replace => {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("trigger \"{}\" for relation \"{relation}\" already exists", new.name),
                ));
            }
            Some((_, true)) => {
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!(
                        "trigger \"{}\" for relation \"{relation}\" is an internal or a child trigger",
                        new.name
                    ),
                ));
            }
            Some((oid, false)) => oid,
            None => self.new_oid(),
        };
        let enabled = match self.triggers.get(&oid) {
            Some(t) => t.enabled,
            None if new.enabled == 0 => b'O',
            None => new.enabled,
        };
        let object = ObjRef::new(PG_TRIGGER, oid);
        self.depends.retain(|d| d.object != object);
        self.depend(object, ObjRef::new(PG_PROC, new.function), DepKind::Normal);
        self.depend(object, ObjRef::new(PG_CLASS, new.relation), DepKind::Auto);
        if new.constraint_relation != 0 {
            self.depend(object, ObjRef::new(PG_CLASS, new.constraint_relation), DepKind::Auto);
        }
        if let Some(constraint) = &constraint {
            self.depend(ObjRef::new(PG_CONSTRAINT, constraint.oid), object, DepKind::Internal);
        }
        for &column in &new.columns {
            self.depend(object, ObjRef::column(new.relation, column), DepKind::Normal);
        }
        self.depend_refs(object, qual);
        if new.parent != 0 {
            self.depend(object, ObjRef::new(PG_TRIGGER, new.parent), DepKind::PartitionPri);
            self.depend(object, ObjRef::new(PG_CLASS, new.relation), DepKind::PartitionSec);
        }
        let NewTrigger {
            name,
            relation,
            function,
            kind,
            args,
            columns,
            old_table,
            new_table,
            qual,
            constraint: _,
            constraint_relation,
            deferrable,
            deferred,
            parent,
            enabled: _,
        } = new;
        let trigger = Trigger {
            oid,
            relation,
            name,
            function,
            kind,
            enabled,
            args,
            columns,
            old_table,
            new_table,
            qual,
            constraint: constraint.as_ref().map_or(0, |c| c.oid),
            constraint_relation,
            deferrable,
            deferred,
            parent,
            ..Trigger::default()
        };
        if let Some(constraint) = constraint {
            self.insert_constraint(constraint);
        }
        self.triggers.insert(oid, trigger);
        self.relation_mut(relation)?.has_triggers = true;
        Ok(oid)
    }

    /// `renametrig_internal`: a new name for a trigger.
    ///
    /// # Errors
    ///
    /// `42710` when the relation of the trigger has a trigger with the new name.
    pub fn rename_trigger(&mut self, oid: u32, name: &str) -> Result<()> {
        let Some(relation) = self.triggers.get(&oid).map(|t| t.relation) else {
            return Err(no_object("trigger", oid));
        };
        if self.trigger_by_name(relation, name).is_some() {
            let rel = self.relation(relation).map_or("", |r| r.name.as_str());
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("trigger \"{name}\" for relation \"{rel}\" already exists"),
            ));
        }
        if let Some(trigger) = self.triggers.get_mut(&oid) {
            trigger.name = name.to_string();
        }
        Ok(())
    }

    /// The part of `DropClonedTriggersFromPartition` that removes the partition dependencies of a clone, so that the clone can go.
    pub fn clear_trigger_parent(&mut self, oid: u32) {
        if let Some(trigger) = self.triggers.get_mut(&oid) {
            trigger.parent = 0;
        }
        self.drop_partition_depends(ObjRef::new(PG_TRIGGER, oid));
    }

    /// `EnableDisableTrigger`: sets `tgenabled` of a trigger.
    pub fn set_trigger_enabled(&mut self, oid: u32, enabled: u8) {
        if let Some(trigger) = self.triggers.get_mut(&oid) {
            trigger.enabled = enabled;
        }
    }

    /// The rules in the order of their OIDs.
    pub fn rules(&self) -> impl Iterator<Item = &Rule> {
        self.rules.values()
    }

    /// The rule with the OID.
    pub fn rule(&self, oid: u32) -> Option<&Rule> {
        self.rules.get(&oid)
    }

    /// The rule of a relation with the name.
    pub fn rule_by_name(&self, relation: u32, name: &str) -> Option<&Rule> {
        self.rules.values().find(|r| r.relation == relation && r.name == name)
    }

    /// The collations in the order of their OIDs.
    pub fn collations(&self) -> impl Iterator<Item = &Collation> {
        self.collations.values()
    }

    /// The collation with the OID.
    pub fn collation(&self, oid: u32) -> Option<&Collation> {
        self.collations.get(&oid)
    }

    /// The collation with the name in a schema, as `lookup_collation` finds it: a collation of the encoding of the database first, then a collation of any encoding.
    pub fn collation_by_name(&self, namespace: u32, name: &str) -> Option<&Collation> {
        let find = |encoding: i32| {
            self.collations
                .values()
                .find(|c| c.namespace == namespace && c.name == name && c.encoding == encoding)
        };
        find(UTF8).or_else(|| find(-1))
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

    /// The casts, in the order of their OIDs.
    pub fn casts(&self) -> impl Iterator<Item = &Cast> {
        self.casts.values()
    }

    /// The rows of `pg_depend` for user objects, in the order that PostgreSQL inserts them.
    pub fn depends(&self) -> &[Depend] {
        &self.depends
    }

    /// The rows of `pg_description` for user objects: each object with its comment, in the order of the objects.
    pub fn descriptions(&self) -> impl Iterator<Item = (ObjRef, &str)> {
        self.descriptions.iter().map(|(object, text)| (*object, text.as_str()))
    }

    /// The comment of an object, or `None`.
    pub fn description(&self, object: ObjRef) -> Option<&str> {
        self.descriptions.get(&object).map(String::as_str)
    }

    /// `CreateComments`: sets the comment of an object, or drops it for `None`.
    pub fn set_description(&mut self, object: ObjRef, text: Option<String>) {
        match text {
            Some(text) => Arc::make_mut(&mut self.descriptions).insert(object, text),
            None if self.descriptions.contains_key(&object) => {
                Arc::make_mut(&mut self.descriptions).remove(&object)
            }
            None => None,
        };
    }

    /// The ACLs that [`Catalog::set_object_acl`] gave.
    pub fn object_acls(&self) -> impl Iterator<Item = (ObjRef, &[String])> {
        self.acls.iter().map(|(object, acl)| (*object, acl.as_slice()))
    }

    /// The ACL that `GRANT` or `REVOKE` gave to an object, or `None` when the object keeps the ACL that it was made with. The ACL of a column is empty when it is null.
    pub fn object_acl(&self, object: ObjRef) -> Option<&[String]> {
        self.acls.get(&object).map(Vec::as_slice)
    }

    /// Sets the ACL of an object, or drops the ACL for `None`.
    pub fn set_object_acl(&mut self, object: ObjRef, acl: Option<Vec<String>>) {
        match acl {
            Some(acl) => Arc::make_mut(&mut self.acls).insert(object, acl),
            None if self.acls.contains_key(&object) => {
                Arc::make_mut(&mut self.acls).remove(&object)
            }
            None => None,
        };
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

    /// `RenameRelationInternal`: the new name of a relation. The row type and the array type get new names, as `RenameTypeInternal` gives them, and the constraint of an index gets the name of the index, as `RenameConstraintById` gives it.
    ///
    /// # Errors
    ///
    /// `42P07` when the schema has a relation with the name, `42710` when the name is the name of a type or of a constraint of the table, and an internal error for a relation that the catalog does not have.
    pub fn rename_relation(&mut self, oid: u32, name: &str) -> Result<()> {
        let rel = self.relation(oid).ok_or_else(|| no_object("relation", oid))?;
        let (namespace, row_type) = (rel.namespace, rel.row_type);
        let table = rel.index.as_ref().map_or(0, |info| info.table);
        if self.relation_by_name(namespace, name).is_some() {
            return Err(duplicate_table(name));
        }
        self.relation_mut(oid)?.name = name.to_string();
        if row_type != 0 {
            self.check_new_type(namespace, name)?;
            let array = self.types.get(&row_type).map_or(0, |t| t.array);
            let array_name = self.array_type_name(name, namespace);
            if let Some(ty) = self.types.get_mut(&row_type) {
                ty.name = name.to_string();
            }
            if let Some(ty) = self.types.get_mut(&array) {
                ty.name = array_name;
            }
        }
        let constraint = self
            .constraints
            .values()
            .find(|c| {
                c.index == oid
                    && c.relation == table
                    && matches!(c.kind, ConKind::Primary | ConKind::Unique)
            })
            .map(|c| c.oid);
        if let Some(constraint) = constraint {
            self.rename_constraint(constraint, name)?;
        }
        Ok(())
    }

    /// `renameatt_internal`: the new name of a column. The caller checks the name.
    ///
    /// # Errors
    ///
    /// An internal error for a column that the catalog does not have.
    pub fn rename_column(&mut self, relation: u32, number: i16, name: &str) -> Result<()> {
        self.column_mut(relation, number)?.name = name.to_string();
        Ok(())
    }

    /// `RenameConstraintById`: the new name of a constraint. The index of a primary key or a unique constraint keeps its name: the caller renames the index, which renames the constraint.
    ///
    /// # Errors
    ///
    /// `42710` when the table has a constraint with the name, and an internal error for a constraint that the catalog does not have.
    pub fn rename_constraint(&mut self, oid: u32, name: &str) -> Result<()> {
        let con = self.constraints.get(&oid).ok_or_else(|| no_object("constraint", oid))?;
        let relation = con.relation;
        if relation != 0 && self.constraints_of(relation).any(|c| c.name == name) {
            let table = self.relation(relation).map_or("", |r| r.name.as_str());
            return Err(duplicate_constraint(name, table));
        }
        if let Some(con) = self.constraints.get_mut(&oid) {
            con.name = name.to_string();
        }
        Ok(())
    }

    /// `RenameSchema`: the new name of a schema. The caller checks the name.
    ///
    /// # Errors
    ///
    /// An internal error for a schema that the catalog does not have.
    pub fn rename_schema(&mut self, oid: u32, name: &str) -> Result<()> {
        let schema = self.schemas.get_mut(&oid).ok_or_else(|| no_object("schema", oid))?;
        schema.name = name.to_string();
        Ok(())
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

    /// Checks the name of a new relation, as `heap_create_with_catalog` and `index_create` do. When the row type of a new table would have the name of an array type that PostgreSQL made, the array type gets a new name, as `moveArrayTypeName` gives it. A type with the name gives the hint of `heap_create_with_catalog`.
    fn check_new_relation(&mut self, namespace: u32, name: &str, row_type: bool) -> Result<()> {
        if self.relation_by_name(namespace, name).is_some() {
            return Err(duplicate_table(name));
        }
        if !row_type {
            return Ok(());
        }
        self.check_new_type(namespace, name).map_err(|e| e.with_hint(
            "A relation has an associated type of the same name, so you must use a name that doesn't conflict with any existing type.",
        ))
    }

    /// Checks the name of a new type: when the name is the name of an array type that PostgreSQL made, the array type gets a new name, as `moveArrayTypeName` gives it. Any other type with the name is an error.
    fn check_new_type(&mut self, namespace: u32, name: &str) -> Result<()> {
        let Some(old) = self.type_by_name(namespace, name).map(|t| (t.oid, t.element)) else {
            return Ok(());
        };
        let is_auto_array = old.1 != 0 && self.types.get(&old.1).is_some_and(|e| e.array == old.0);
        if !is_auto_array {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("type \"{name}\" already exists"),
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

    /// `ATExecAddColumn` for the catalog: the column goes at the end of the columns of the table, and it depends on its type and its collation, as `add_column_datatype_dependency` and `add_column_collation_dependency` record them. The result is the number of the new column.
    ///
    /// # Errors
    ///
    /// `54011` for a table that has the most columns already, and an internal error for a relation that is not in the catalog.
    pub fn add_column(&mut self, table: u32, column: Column) -> Result<i16> {
        let refs = [(PG_TYPE, column.ty), (PG_COLLATION, column.collation)];
        let rel = self.relation_mut(table)?;
        if rel.columns.len() >= MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {MAX_COLUMNS} columns"),
            ));
        }
        rel.columns.push(column);
        let number = i16::try_from(rel.columns.len()).unwrap_or(i16::MAX);
        for (class, found) in refs.into_iter().filter(|&(_, found)| found != 0) {
            self.depend(ObjRef::column(table, number), ObjRef::new(class, found), DepKind::Normal);
        }
        Ok(number)
    }

    /// `ATExecAlterColumnType` for the catalog: the column gets its new type, type modifier, collation and number of array dimensions, and its dependencies on the old type and collation give way to dependencies on the new ones.
    ///
    /// # Errors
    ///
    /// An internal error for a column that is not in the catalog.
    pub fn alter_column_type(&mut self, table: u32, number: i16, new: &Column) -> Result<()> {
        let column = self.column_mut(table, number)?;
        column.ty = new.ty;
        column.typmod = new.typmod;
        column.collation = new.collation;
        column.ndims = new.ndims;
        let me = ObjRef::column(table, number);
        self.depends.retain(|d| {
            d.object != me
                || d.kind != DepKind::Normal
                || !matches!(d.referenced.class, PG_TYPE | PG_COLLATION)
        });
        let refs = [(PG_TYPE, new.ty), (PG_COLLATION, new.collation)];
        for (class, found) in refs.into_iter().filter(|&(_, found)| found != 0) {
            self.depend(me, ObjRef::new(class, found), DepKind::Normal);
        }
        Ok(())
    }

    /// The column `column` of an index, from 0, after a change of the type of the column `from` of the table, as the index that `ATPostAlterTypeCleanup` makes again has it: the type of the table column, the collation `collation`, and for a key column the operator class `class`.
    ///
    /// # Errors
    ///
    /// An internal error for an index or a column that is not in the catalog.
    pub fn set_index_column(
        &mut self,
        index: u32,
        column: usize,
        class: u32,
        collation: u32,
        from: &Column,
    ) -> Result<()> {
        let rel = self.relation_mut(index)?;
        let found = rel.columns.get_mut(column).map(|c| {
            c.ty = from.ty;
            c.typmod = from.typmod;
            c.ndims = from.ndims;
            c.collation = collation;
        });
        if let Some(info) = rel.index.as_mut()
            && let (Some(class_at), Some(collation_at)) =
                (info.classes.get_mut(column), info.collations.get_mut(column))
        {
            *class_at = class;
            *collation_at = collation;
        }
        found.ok_or_else(|| no_object(&format!("column {column} of index"), index))
    }

    /// The equality operators of a foreign key, `conpfeqop`, `conppeqop` and `conffeqop`, after a change of the type of a key column, as the foreign key that `ATPostAlterTypeCleanup` makes again has them.
    ///
    /// # Errors
    ///
    /// An internal error for a constraint that is not a foreign key of the catalog.
    pub fn set_foreign_operators(
        &mut self,
        constraint: u32,
        pf_eq: Vec<u32>,
        pp_eq: Vec<u32>,
        ff_eq: Vec<u32>,
    ) -> Result<()> {
        let foreign = self.constraints.get_mut(&constraint).and_then(|c| c.foreign.as_mut());
        let Some(foreign) = foreign else {
            return Err(no_object("foreign key", constraint));
        };
        foreign.pf_eq = pf_eq;
        foreign.pp_eq = pp_eq;
        foreign.ff_eq = ff_eq;
        Ok(())
    }

    /// `heap_create_with_catalog` for an ordinary table: the table gets an OID, then its array type, then its row type.
    pub fn create_table(&mut self, new: NewRelation) -> Result<u32> {
        self.create_relation(new, RelKind::Table)
    }

    /// `heap_create_with_catalog` for a partitioned table. [`Catalog::set_partition_key`] then gives the key.
    pub fn create_partitioned_table(&mut self, new: NewRelation) -> Result<u32> {
        self.create_relation(new, RelKind::Partitioned)
    }

    /// `StorePartitionKey`: the row of `pg_partitioned_table`. The table depends on the operator class and the collation of each key column.
    pub fn set_partition_key(&mut self, table: u32, key: PartitionKey) -> Result<()> {
        // `DEFAULT_COLLATION_OID`: the table does not depend on it.
        const DEFAULT_COLLATION: u32 = 100;
        let class = ObjRef::new(PG_CLASS, table);
        let classes = key.classes.iter().map(|&c| ObjRef::new(PG_OPCLASS, c));
        let collations = key
            .collations
            .iter()
            .filter(|&&c| c != 0 && c != DEFAULT_COLLATION)
            .map(|&c| ObjRef::new(PG_COLLATION, c));
        let refs: Vec<ObjRef> = classes.chain(collations).collect();
        let Some(rel) = self.relations.get_mut(&table) else {
            return Err(no_object("relation", table));
        };
        rel.partition_key = Some(key);
        for found in refs {
            self.depend(class, found, DepKind::Normal);
        }
        Ok(())
    }

    /// `StorePartitionBound` and `StoreCatalogInheritance1` for a partition: the bound, the row of `pg_inherits`, and the dependency of the partition on its parent. The dependency is automatic, so `DROP TABLE` of the parent drops the partition too.
    pub fn attach_partition(
        &mut self,
        table: u32,
        parent: u32,
        bound: PartitionBound,
    ) -> Result<()> {
        let Some(rel) = self.relations.get_mut(&table) else {
            return Err(no_object("relation", table));
        };
        rel.partition_of = Some(PartitionOf { parent, bound });
        self.depend(ObjRef::new(PG_CLASS, table), ObjRef::new(PG_CLASS, parent), DepKind::Auto);
        Ok(())
    }

    /// `StoreCatalogInheritance`: the rows of `pg_inherits` of a new table of `INHERITS`, in the order of its parents, and the normal dependency of the table on each parent. So `DROP TABLE` of a parent needs `CASCADE`.
    pub fn set_inherits(&mut self, table: u32, parents: Vec<u32>) -> Result<()> {
        let me = ObjRef::new(PG_CLASS, table);
        for &parent in &parents {
            self.depend(me, ObjRef::new(PG_CLASS, parent), DepKind::Normal);
        }
        self.relation_mut(table)?.inherits = parents;
        Ok(())
    }

    /// `StoreCatalogInheritance1` of `ALTER TABLE ... INHERIT`: the table gets the parent after its other parents, with the next `inhseqno`, and the normal dependency on the parent.
    pub fn add_inherit(&mut self, table: u32, parent: u32) -> Result<()> {
        self.depend(ObjRef::new(PG_CLASS, table), ObjRef::new(PG_CLASS, parent), DepKind::Normal);
        self.relation_mut(table)?.inherits.push(parent);
        Ok(())
    }

    /// The new `reloftype` of the table, or 0 for `NOT OF`, as `ATExecAddOf` and `ATExecDropOf` set it. The table loses its dependency on the old type and gets a normal dependency on the new type, as `heap_create_with_catalog` records it for a new typed table.
    pub fn set_of_type(&mut self, table: u32, ty: u32) -> Result<()> {
        let rel = self.relation_mut(table)?;
        let old = std::mem::replace(&mut rel.of_type, ty);
        let me = ObjRef::new(PG_CLASS, table);
        if old != 0 {
            let old = ObjRef::new(PG_TYPE, old);
            self.depends
                .retain(|d| d.object != me || d.referenced != old || d.kind != DepKind::Normal);
        }
        if ty != 0 {
            self.depend(me, ObjRef::new(PG_TYPE, ty), DepKind::Normal);
        }
        Ok(())
    }

    /// The part of `RemoveInheritance` of `ALTER TABLE ... NO INHERIT`: the table loses the row of `pg_inherits` of the parent and its dependency on the parent. The next `INHERIT` takes the number after the highest number that is left.
    pub fn remove_inherit(&mut self, table: u32, parent: u32) -> Result<()> {
        let rel = self.relation_mut(table)?;
        for p in &mut rel.inherits {
            if *p == parent {
                *p = 0;
            }
        }
        while rel.inherits.last() == Some(&0) {
            rel.inherits.pop();
        }
        let me = ObjRef::new(PG_CLASS, table);
        let parent = ObjRef::new(PG_CLASS, parent);
        self.depends
            .retain(|d| d.object != me || d.referenced != parent || d.kind != DepKind::Normal);
        Ok(())
    }

    /// `find_inheritance_children`: the partitions of a table and the tables that inherit from it, in the order of their OIDs.
    pub fn children(&self, parent: u32) -> Vec<&Relation> {
        let child = |r: &&Relation| {
            r.partition_of.as_ref().is_some_and(|p| p.parent == parent)
                || r.inherits.contains(&parent)
        };
        self.relations.values().filter(child).collect()
    }

    /// `find_all_inheritors` with no table itself: the children of the table, then the children of each of them in the same order, and so on. A table that inherits from two of them comes once.
    pub fn all_inheritors(&self, table: u32) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        let mut at = 0;
        let mut parent = table;
        loop {
            for child in self.children(parent) {
                if child.oid != table && !out.contains(&child.oid) {
                    out.push(child.oid);
                }
            }
            let Some(&next) = out.get(at) else { break };
            parent = next;
            at += 1;
        }
        out
    }

    /// The partitions of a partitioned table, in the order of their OIDs.
    pub fn partitions(&self, parent: u32) -> Vec<&Relation> {
        self.relations
            .values()
            .filter(|r| r.partition_of.as_ref().is_some_and(|p| p.parent == parent))
            .collect()
    }

    /// `IndexSetParentIndex`: the index becomes a partition of the index `parent` of the parent table, with the partition dependencies on `parent` and on its table.
    pub fn set_index_parent(&mut self, index: u32, parent: u32) -> Result<()> {
        let rel = self.relation_mut(index)?;
        let table = rel.index.as_ref().map_or(0, |i| i.table);
        if let Some(info) = rel.index.as_mut() {
            info.parent = parent;
        }
        let me = ObjRef::new(PG_CLASS, index);
        self.depend(me, ObjRef::new(PG_CLASS, parent), DepKind::PartitionPri);
        self.depend(me, ObjRef::new(PG_CLASS, table), DepKind::PartitionSec);
        Ok(())
    }

    /// `ConstraintSetParentConstraint`: the constraint of a partition comes from the constraint `parent` of the parent table, with the partition dependencies on `parent` and on the table of the constraint.
    pub fn set_constraint_parent(&mut self, constraint: u32, parent: u32) -> Result<()> {
        let con = self
            .constraints
            .get_mut(&constraint)
            .ok_or_else(|| no_object("constraint", constraint))?;
        con.parent = parent;
        con.local = false;
        con.inhcount = con.inhcount.saturating_add(1);
        con.no_inherit = false;
        let table = con.relation;
        let me = ObjRef::new(PG_CONSTRAINT, constraint);
        self.depend(me, ObjRef::new(PG_CONSTRAINT, parent), DepKind::PartitionPri);
        self.depend(me, ObjRef::new(PG_CLASS, table), DepKind::PartitionSec);
        Ok(())
    }

    /// Sets `indisvalid` of an index.
    pub fn set_index_valid(&mut self, index: u32, valid: bool) -> Result<()> {
        if let Some(info) = self.relation_mut(index)?.index.as_mut() {
            info.valid = valid;
        }
        Ok(())
    }

    /// Sets `conislocal` and `coninhcount` of a constraint that a parent table gives.
    pub fn set_constraint_inherited(
        &mut self,
        constraint: u32,
        local: bool,
        inhcount: i16,
    ) -> Result<()> {
        let Some(con) = self.constraints.get_mut(&constraint) else {
            return Err(no_object("constraint", constraint));
        };
        con.local = local;
        con.inhcount = inhcount;
        Ok(())
    }

    /// Sets `attislocal` and `attinhcount` of the column `number` of a table.
    pub fn set_column_inherited(
        &mut self,
        table: u32,
        number: i16,
        local: bool,
        inhcount: i16,
    ) -> Result<()> {
        let rel = self.relation_mut(table)?;
        let at = usize::try_from(number).ok().and_then(|n| n.checked_sub(1));
        let Some(column) = at.and_then(|at| rel.columns.get_mut(at)) else {
            return Err(no_object("column", u32::try_from(number).unwrap_or_default()));
        };
        column.local = local;
        column.inhcount = inhcount;
        Ok(())
    }

    /// The part of `RemoveInheritance` and `DetachPartitionFinalize` for the partition `table`: it loses its bound, its row of `pg_inherits` and its dependency on its parent.
    pub fn detach_partition(&mut self, table: u32) -> Result<()> {
        let Some(of) = self.relation_mut(table)?.partition_of.take() else { return Ok(()) };
        let me = ObjRef::new(PG_CLASS, table);
        let parent = ObjRef::new(PG_CLASS, of.parent);
        self.depends
            .retain(|d| d.object != me || d.referenced != parent || d.kind != DepKind::Auto);
        Ok(())
    }

    /// `IndexSetParentIndex` with no parent: the index is no longer a partition of an index, and it loses its partition dependencies.
    pub fn clear_index_parent(&mut self, index: u32) -> Result<()> {
        if let Some(info) = self.relation_mut(index)?.index.as_mut() {
            info.parent = 0;
        }
        self.drop_partition_depends(ObjRef::new(PG_CLASS, index));
        Ok(())
    }

    /// `ConstraintSetParentConstraint` with no parent: the constraint is local again, with no parent constraint and no partition dependencies.
    pub fn clear_constraint_parent(&mut self, constraint: u32) -> Result<()> {
        let con = self
            .constraints
            .get_mut(&constraint)
            .ok_or_else(|| no_object("constraint", constraint))?;
        con.parent = 0;
        con.local = true;
        con.inhcount = con.inhcount.saturating_sub(1);
        self.drop_partition_depends(ObjRef::new(PG_CONSTRAINT, constraint));
        Ok(())
    }

    /// `deleteDependencyRecordsForClass` for the partition dependencies of an object.
    fn drop_partition_depends(&mut self, object: ObjRef) {
        self.depends.retain(|d| {
            d.object != object || !matches!(d.kind, DepKind::PartitionPri | DepKind::PartitionSec)
        });
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
                labels: None,
                range: None,
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
                labels: None,
                range: None,
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
                replica_identity: 0,
                partition_key: None,
                partition_of: None,
                inherits: Vec::new(),
                of_type: 0,
                unlogged: false,
            },
        );
        let (class, ty) = (ObjRef::new(PG_CLASS, oid), ObjRef::new(PG_TYPE, row_type));
        let namespace = ObjRef::new(PG_NAMESPACE, new.namespace);
        // The type of `CREATE TYPE ... AS` is the main object, so the relation is part of the type and the type depends on the schema.
        if kind == RelKind::Composite {
            self.depend(ty, namespace, DepKind::Normal);
            self.depend(class, ty, DepKind::Internal);
        } else {
            self.depend(ty, class, DepKind::Internal);
        }
        self.depend(ObjRef::new(PG_TYPE, array), ty, DepKind::Internal);
        for (column, found) in column_refs {
            self.depend(column, found, DepKind::Normal);
        }
        if kind != RelKind::Composite {
            self.depend(class, namespace, DepKind::Normal);
        }
        Ok(oid)
    }

    /// `DefineCompositeType`: the relation of a composite type, with its row type and the array type, as a table gets them. The result is the OID of the row type.
    pub fn create_composite_type(&mut self, new: NewRelation) -> Result<u32> {
        let oid = self.create_relation(new, RelKind::Composite)?;
        Ok(self.relation(oid).map_or(0, |r| r.row_type))
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
        self.relation_mut(oid)?.view = Some(ViewInfo { rule, text, path, populated: true });
        self.depend_rule(rule, oid, refs);
        Ok(oid)
    }

    /// `StoreViewQuery` for `CREATE MATERIALIZED VIEW`: the new table `oid` of `create_ctas_internal` becomes a materialized view, and it gets its `_RETURN` rule as a view does. `populated` is false for `WITH NO DATA`.
    pub fn make_matview(
        &mut self,
        oid: u32,
        text: String,
        path: Vec<u32>,
        refs: &[ObjRef],
        populated: bool,
    ) -> Result<()> {
        let rule = self.new_oid();
        let rel = self.relation_mut(oid)?;
        rel.kind = RelKind::MatView;
        rel.view = Some(ViewInfo { rule, text, path, populated });
        self.depend_rule(rule, oid, refs);
        Ok(())
    }

    /// `SetMatViewPopulatedState`: the new `relispopulated` of the materialized view `oid`.
    pub fn set_populated(&mut self, oid: u32, populated: bool) -> Result<()> {
        let view = self.relation_mut(oid)?.view.as_mut().ok_or_else(|| no_object("view", oid))?;
        view.populated = populated;
        Ok(())
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

    /// A new text of the view `oid` with the same query, which `ALTER ... RENAME` gives when the view reads an object that gets a new name. The rule keeps its dependencies.
    pub fn set_view_text(&mut self, oid: u32, text: String, path: Vec<u32>) -> Result<()> {
        let view = self.relation_mut(oid)?.view.as_mut().ok_or_else(|| no_object("view", oid))?;
        view.text = text;
        view.path = path;
        Ok(())
    }

    /// `InsertRule`: a new rule, or with `replace` new facts for the rule of the relation with the same name. A replaced rule keeps its OID and `ev_enabled`, and gets new dependencies. The rule depends on its relation, then on the objects that the actions read, then on the objects that the condition reads. The result is the OID of the rule.
    ///
    /// # Errors
    ///
    /// `42710` when the relation has a rule with the name and `replace` is false.
    pub fn create_rule(
        &mut self,
        new: NewRule,
        replace: bool,
        actions: &[ObjRef],
        qual: &[ObjRef],
    ) -> Result<u32> {
        let old = self.rule_by_name(new.relation, &new.name).map(|r| r.oid);
        let oid = match old {
            Some(_) if !replace => {
                let relation = self.relation(new.relation).map_or("", |r| r.name.as_str());
                return Err(Error::new(
                    SqlState::DUPLICATE_OBJECT,
                    format!("rule \"{}\" for relation \"{relation}\" already exists", new.name),
                ));
            }
            Some(oid) => oid,
            None => self.new_oid(),
        };
        let enabled = self.rules.get(&oid).map_or(b'O', |r| r.enabled);
        let object = ObjRef::new(PG_REWRITE, oid);
        self.depends.retain(|d| d.object != object);
        let kind = if new.event == b'1' { DepKind::Internal } else { DepKind::Auto };
        self.depend(object, ObjRef::new(PG_CLASS, new.relation), kind);
        self.depend_refs(object, actions);
        self.depend_refs(object, qual);
        let NewRule { name, relation, event, instead, qualified, text, path } = new;
        let rule = Rule { oid, name, relation, event, instead, enabled, qualified, text, path };
        self.rules.insert(oid, rule);
        Ok(oid)
    }

    /// `CollationCreate` with `if_not_exists` and `quiet`: adds a collation and gives its OID, or gives `None` and adds nothing when the schema has a collation with the name for the same encoding. A collation for any encoding also conflicts with a collation for the encoding of the database, and a collation for one encoding conflicts with a collation for any encoding. The OID in `collation` is not used, and `comment` is the description of the new collation.
    pub fn create_collation(&mut self, collation: Collation, comment: Option<&str>) -> Option<u32> {
        let conflict = self.collations.values().any(|c| {
            c.namespace == collation.namespace
                && c.name == collation.name
                && collation_conflict(c.encoding, collation.encoding)
        });
        if conflict {
            return None;
        }
        let oid = self.new_oid();
        Arc::make_mut(&mut self.collations).insert(oid, Collation { oid, ..collation });
        if let Some(comment) = comment {
            self.set_description(ObjRef::new(PG_COLLATION, oid), Some(comment.to_string()));
        }
        Some(oid)
    }

    /// The dependencies of the `_RETURN` rule of a view, as `InsertRule` and `recordDependencyOnExpr` record them. The rule is part of the view, and it depends on each object that the query reads. The objects come in the order of `eliminate_duplicate_dependencies`, from the highest OID down, and a column takes the place of its whole relation.
    fn depend_rule(&mut self, rule: u32, view: u32, refs: &[ObjRef]) {
        let object = ObjRef::new(PG_REWRITE, rule);
        self.depend(object, ObjRef::new(PG_CLASS, view), DepKind::Internal);
        self.depend_refs(object, refs);
    }

    /// `recordDependencyOnExpr` with `DEPENDENCY_NORMAL`: the object depends on each object of `refs`. The objects come in the order of `eliminate_duplicate_dependencies`, from the highest OID down, and a column takes the place of its whole relation.
    fn depend_refs(&mut self, object: ObjRef, refs: &[ObjRef]) {
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

    /// `mark_index_clustered`: sets `indisclustered` on the index and clears it on each other index of the table. `None` clears it on all of them.
    pub fn set_clustered(&mut self, table: u32, index: Option<u32>) {
        for rel in self.relations.values_mut() {
            if let Some(info) = rel.index.as_mut().filter(|i| i.table == table) {
                info.clustered = Some(rel.oid) == index;
            }
        }
    }

    /// `relation_mark_replica_identity`: sets `relreplident` of the table, and sets `indisreplident` on the index and clears it on each other index of the table.
    pub fn set_replica_identity(&mut self, table: u32, code: u8, index: Option<u32>) -> Result<()> {
        self.relation_mut(table)?.replica_identity = code;
        for rel in self.relations.values_mut() {
            if let Some(info) = rel.index.as_mut().filter(|i| i.table == table) {
                info.replica_identity = Some(rel.oid) == index;
            }
        }
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
                labels: None,
                range: None,
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
                labels: None,
                range: None,
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

    /// `DefineEnum`: the enum type, its labels and its array type. The array type gets the first OID, then the type, then each label gets the next even OID, as `EnumValuesCreate` gives the labels even OIDs in their sort order. The type depends on its schema. The return value is the OID of the type.
    ///
    /// # Errors
    ///
    /// `42710` when the schema has a type with the name that is not an array type of another type.
    pub fn create_enum(&mut self, new: NewEnum) -> Result<u32> {
        self.check_new_type(new.namespace, &new.name)?;
        let array = self.new_oid();
        let oid = self.new_oid();
        let mut labels = Vec::with_capacity(new.labels.len());
        for (i, label) in new.labels.into_iter().enumerate() {
            let mut label_oid = self.new_oid();
            while label_oid & 1 != 0 {
                label_oid = self.new_oid();
            }
            labels.push(EnumLabel { oid: label_oid, sort_order: (i + 1) as f32, label });
        }
        let array_name = self.array_type_name(&new.name, new.namespace);
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
                domain: None,
                labels: Some(labels),
                range: None,
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
                labels: None,
                range: None,
            },
        );
        let me = ObjRef::new(PG_TYPE, oid);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        self.depend(ObjRef::new(PG_TYPE, array), me, DepKind::Internal);
        self.uncommitted_types.push(oid);
        Ok(oid)
    }

    /// `DefineRange`: the range type, its multirange type, their array types, the constructors and the cast from the range type to the multirange type. The array type of the range type gets the first OID, then the multirange type, its array type and the range type. The constructors and the cast come after the types. The return value is the OID of the range type.
    ///
    /// # Errors
    ///
    /// `42710` when the schema has a type with the name of the range type or of the multirange type that is not an array type of another type.
    pub fn create_range(&mut self, new: NewRange) -> Result<u32> {
        self.check_new_type(new.namespace, &new.name)?;
        let array = self.new_oid();
        let multirange = self.new_oid();
        let multirange_array = self.new_oid();
        let oid = self.new_oid();
        let mut info = new.info;
        info.multirange = multirange;
        let facts = info.clone();
        let type_of = |oid: u32, name: String, namespace: u32| Type {
            oid,
            name,
            namespace,
            owner: new.owner,
            relation: 0,
            element: 0,
            array: 0,
            domain: None,
            labels: None,
            range: None,
        };
        let me = ObjRef::new(PG_TYPE, oid);
        let multi = ObjRef::new(PG_TYPE, multirange);
        let range = Type {
            array,
            range: Some(RangePart::Range(info)),
            ..type_of(oid, new.name.clone(), new.namespace)
        };
        self.types.insert(oid, range);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        self.check_new_type(new.multirange_namespace, &new.multirange_name)?;
        let multirange_type = Type {
            array: multirange_array,
            range: Some(RangePart::Multirange(oid)),
            ..type_of(multirange, new.multirange_name.clone(), new.multirange_namespace)
        };
        self.types.insert(multirange, multirange_type);
        self.depend(multi, ObjRef::new(PG_NAMESPACE, new.multirange_namespace), DepKind::Normal);
        let array_name = self.array_type_name(&new.name, new.namespace);
        self.types
            .insert(array, Type { element: oid, ..type_of(array, array_name, new.namespace) });
        self.depend(ObjRef::new(PG_TYPE, array), me, DepKind::Internal);
        // `DefineRange` checks the name of the array type of the multirange type in the schema of the range type.
        let multirange_array_name = self.array_type_name(&new.multirange_name, new.namespace);
        let multirange_array_type = Type {
            element: multirange,
            ..type_of(multirange_array, multirange_array_name, new.multirange_namespace)
        };
        self.types.insert(multirange_array, multirange_array_type);
        self.depend(ObjRef::new(PG_TYPE, multirange_array), multi, DepKind::Internal);
        let constructor = |name: &str, src: &str, rettype: u32, argtypes: Vec<u32>| Function {
            oid: 0,
            name: name.to_string(),
            namespace: new.namespace,
            owner: BOOTSTRAP_SUPERUSER,
            lang: INTERNAL_LANGUAGE,
            cost: 1,
            rows: 0,
            support: 0,
            kind: b'f',
            // The constructors of a range type accept null bounds, which are infinite.
            strict: rettype == multirange,
            leakproof: false,
            volatile: b'i',
            parallel: b's',
            rettype,
            retset: false,
            argtypes,
            allargtypes: Vec::new(),
            argmodes: Vec::new(),
            argnames: Vec::new(),
            argdefaults: None,
            nargdefaults: 0,
            src: src.to_string(),
            bin: String::new(),
            body: None,
        };
        let multirange_name = new.multirange_name.as_str();
        let variadic = Function {
            allargtypes: vec![array],
            argmodes: vec![b'v'],
            ..constructor(multirange_name, "multirange_constructor2", multirange, vec![array])
        };
        let functions = [
            (constructor(&new.name, "range_constructor2", oid, vec![facts.subtype; 2]), me),
            (
                constructor(
                    &new.name,
                    "range_constructor3",
                    oid,
                    vec![facts.subtype, facts.subtype, TEXT],
                ),
                me,
            ),
            (
                constructor(multirange_name, "multirange_constructor0", multirange, Vec::new()),
                multi,
            ),
            (constructor(multirange_name, "multirange_constructor1", multirange, vec![oid]), multi),
            (variadic, multi),
        ];
        let mut constructors = [0; 5];
        for (slot, (function, owner)) in constructors.iter_mut().zip(functions) {
            *slot = self.insert_constructor(function, owner);
        }
        // `RangeCreate`: the range type depends on the objects of its row of `pg_range`, and the multirange type is part of it.
        let refs = [
            ObjRef::new(PG_TYPE, facts.subtype),
            ObjRef::new(PG_OPCLASS, facts.subopc),
            ObjRef::new(PG_COLLATION, facts.collation),
            ObjRef::new(PG_PROC, facts.canonical),
            ObjRef::new(PG_PROC, facts.subdiff),
        ];
        for referenced in refs {
            if referenced.oid != 0 {
                self.depend(me, referenced, DepKind::Normal);
            }
        }
        self.depend(multi, me, DepKind::Internal);
        if let Some(Type { range: Some(RangePart::Range(info)), .. }) = self.types.get_mut(&oid) {
            info.constructors = constructors;
        }
        let cast = self.new_oid();
        let row = Cast {
            oid: cast,
            source: oid,
            target: multirange,
            func: constructors[3],
            context: b'e',
            method: b'f',
        };
        self.casts.insert(cast, row);
        let object = ObjRef::new(PG_CAST, cast);
        for referenced in [me, multi, ObjRef::new(PG_PROC, constructors[3])] {
            self.depend(object, referenced, DepKind::Internal);
        }
        Ok(oid)
    }

    /// `ProcedureCreate` for a constructor of a range type or of a multirange type: the function depends on its schema and on the types of its result and its arguments, and it is part of the type `owner`. The return value is the OID of the function.
    fn insert_constructor(&mut self, mut function: Function, owner: ObjRef) -> u32 {
        let oid = self.new_oid();
        function.oid = oid;
        let me = ObjRef::new(PG_PROC, oid);
        self.depend(me, ObjRef::new(PG_NAMESPACE, function.namespace), DepKind::Normal);
        let mut types = vec![function.rettype];
        for &ty in function.argtypes.iter().chain(&function.allargtypes) {
            if !types.contains(&ty) {
                types.push(ty);
            }
        }
        for ty in types {
            self.depend(me, ObjRef::new(PG_TYPE, ty), DepKind::Normal);
        }
        self.depend(me, owner, DepKind::Internal);
        self.functions.insert(oid, function);
        oid
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

    /// `ProcedureCreate` with `replace` for the function `oid` that exists: the function gets the new facts and keeps its OID and its owner. Its old dependencies go, and it depends on its schema and on the objects in `refs`, as a new function does.
    ///
    /// # Errors
    ///
    /// An internal error when no function has the OID.
    pub fn replace_function(&mut self, oid: u32, mut new: Function, refs: &[ObjRef]) -> Result<()> {
        let old = self.functions.get(&oid).ok_or_else(|| no_object("function", oid))?;
        new.oid = oid;
        new.owner = old.owner;
        let me = ObjRef::new(PG_PROC, oid);
        self.depends.retain(|d| d.object != me);
        self.depend(me, ObjRef::new(PG_NAMESPACE, new.namespace), DepKind::Normal);
        for &referenced in refs {
            self.depend(me, referenced, DepKind::Normal);
        }
        self.functions.insert(oid, new);
        Ok(())
    }

    /// `AlterFunction`, and `AlterObjectRename_internal`, `AlterObjectNamespace_internal` and `AlterObjectOwner_internal` for a function: `change` gives the new row of the function. `changeDependencyFor` changes the dependency of the function on its schema when the schema changes.
    ///
    /// # Errors
    ///
    /// An internal error for a function that the catalog does not have.
    pub fn alter_function(&mut self, oid: u32, change: impl FnOnce(&mut Function)) -> Result<()> {
        let function = self.functions.get_mut(&oid).ok_or_else(|| no_object("function", oid))?;
        let old = function.namespace;
        change(function);
        let new = function.namespace;
        if old != new {
            self.change_namespace_dependency(ObjRef::new(PG_PROC, oid), old, new);
        }
        Ok(())
    }

    /// `CreateExtensionInternal`: a new extension, which gets the next OID.
    ///
    /// # Errors
    ///
    /// `42710` when an extension has the name.
    pub fn create_extension(&mut self, mut new: Extension) -> Result<u32> {
        if self.extensions.values().any(|e| e.name == new.name) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("extension \"{}\" already exists", new.name),
            ));
        }
        let oid = self.new_oid();
        new.oid = oid;
        self.depend(
            ObjRef::new(PG_EXTENSION, oid),
            ObjRef::new(PG_NAMESPACE, new.namespace),
            DepKind::Normal,
        );
        self.extensions.insert(oid, new);
        Ok(oid)
    }

    /// `recordDependencyOnCurrentExtension`: makes an object a member of an extension, as a statement of the script of the extension does.
    pub fn add_to_extension(&mut self, object: ObjRef, extension: u32) {
        self.depend(object, ObjRef::new(PG_EXTENSION, extension), DepKind::Extension);
    }

    /// `create_proc_lang`: a new procedural language, which gets the next OID. It depends on its handlers.
    ///
    /// # Errors
    ///
    /// `42710` when a language has the name.
    pub fn create_language(&mut self, mut new: Language) -> Result<u32> {
        if self.languages.values().any(|l| l.name == new.name) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("language \"{}\" already exists", new.name),
            ));
        }
        let oid = self.new_oid();
        new.oid = oid;
        let me = ObjRef::new(PG_LANGUAGE, oid);
        for handler in [new.handler, new.inline, new.validator] {
            if handler != 0 {
                self.depend(me, ObjRef::new(PG_PROC, handler), DepKind::Normal);
            }
        }
        self.languages.insert(oid, new);
        Ok(oid)
    }

    /// The procedural languages, in the order of their OIDs.
    pub fn languages(&self) -> impl Iterator<Item = &Language> {
        self.languages.values()
    }

    /// The extensions, in the order of their OIDs.
    pub fn extensions(&self) -> impl Iterator<Item = &Extension> {
        self.extensions.values()
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
            Some(name) => self.new_domain_constraint_name(domain, &domain_name, name)?,
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

    /// The name that a statement gives to a constraint of a domain, which no other constraint of the domain has.
    fn new_domain_constraint_name(
        &self,
        domain: u32,
        domain_name: &str,
        name: &str,
    ) -> Result<String> {
        if self.constraints_of_domain(domain).any(|c| c.name == name) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("constraint \"{name}\" for domain \"{domain_name}\" already exists"),
            ));
        }
        Ok(name.to_string())
    }

    /// `domainAddNotNullConstraint`: the not-null constraint of a domain, which also sets `typnotnull`. Without a name, the name is `domain_not_null`.
    pub fn add_domain_not_null(&mut self, domain: u32, name: Option<&str>) -> Result<u32> {
        let ty = self.types.get(&domain).ok_or_else(|| no_object("type", domain))?;
        let (domain_name, namespace) = (ty.name.clone(), ty.namespace);
        let name = match name {
            Some(name) => self.new_domain_constraint_name(domain, &domain_name, name)?,
            None => self.choose_constraint_name(&domain_name, None, "not_null", namespace),
        };
        let mut constraint = self.new_constraint(name, namespace, ConKind::NotNull, 0, Vec::new());
        constraint.domain = domain;
        let me = ObjRef::new(PG_CONSTRAINT, constraint.oid);
        let oid = constraint.oid;
        self.insert_constraint(constraint);
        self.depend(me, ObjRef::new(PG_TYPE, domain), DepKind::Auto);
        if let Some(Type { domain: Some(facts), .. }) = self.types.get_mut(&domain) {
            facts.not_null = true;
        }
        Ok(oid)
    }

    /// Sets `relacl` of a relation.
    pub fn set_acl(&mut self, oid: u32, acl: Option<Vec<String>>) -> Result<()> {
        self.relation_mut(oid)?.acl = acl;
        Ok(())
    }

    /// `DefineSequence`. A sequence has no row type, and each column of a sequence is not null.
    pub fn create_sequence(&mut self, mut new: NewRelation, mut info: SequenceInfo) -> Result<u32> {
        self.check_new_relation(new.namespace, &new.name, false)?;
        for column in &mut new.columns {
            column.not_null = true;
        }
        let oid = self.new_oid();
        info.file = oid;
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
                replica_identity: 0,
                partition_key: None,
                partition_of: None,
                inherits: Vec::new(),
                of_type: 0,
                unlogged: false,
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

    /// `ATExecSetRelOptions` of `SET LOGGED` and `SET UNLOGGED`: the persistence of the relation changes. For a table, its indexes, its TOAST table with the index of the TOAST table, and the sequences that it owns change with it.
    ///
    /// # Errors
    ///
    /// The relation does not exist.
    pub fn set_unlogged(&mut self, oid: u32, unlogged: bool) -> Result<()> {
        let rel = self.relation(oid).ok_or_else(|| no_object("relation", oid))?;
        let mut change = vec![oid];
        if rel.sequence.is_none() {
            let toast = rel.toast;
            change.extend(self.owned_sequences(oid));
            for (&index, rel) in &self.relations {
                if rel
                    .index
                    .as_ref()
                    .is_some_and(|i| i.table == oid || (toast != 0 && i.table == toast))
                {
                    change.push(index);
                }
            }
            if toast != 0 {
                change.push(toast);
            }
        }
        for oid in change {
            self.relation_mut(oid)?.unlogged = unlogged;
        }
        Ok(())
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

    /// Changes the expressions that the catalog keeps in the form of the analyzer: the defaults of the columns and of the domains, the expressions of the check constraints, and the expressions and the predicates of the indexes. `change` gets the form, and true for the list of the expressions of an index. It gives the new form, or `None` for no change.
    ///
    /// # Errors
    ///
    /// The errors of `change`.
    pub fn change_stored_exprs(
        &mut self,
        change: &mut dyn FnMut(&str, bool) -> Result<Option<String>>,
    ) -> Result<()> {
        for default in self.defaults.values_mut() {
            if let Some(new) = change(&default.expr, false)? {
                default.expr = new;
            }
        }
        for con in self.constraints.values_mut() {
            if let Some(expr) = &mut con.expr
                && let Some(new) = change(expr, false)?
            {
                *expr = new;
            }
        }
        for ty in self.types.values_mut() {
            if let Some(expr) = ty.domain.as_mut().and_then(|d| d.default.as_mut())
                && let Some(new) = change(expr, false)?
            {
                *expr = new;
            }
        }
        for info in self.relations.values_mut().filter_map(|r| r.index.as_mut()) {
            if let Some(exprs) = &mut info.exprs
                && let Some(new) = change(exprs, true)?
            {
                *exprs = new;
            }
            if let Some(predicate) = &mut info.predicate
                && let Some(new) = change(predicate, false)?
            {
                *predicate = new;
            }
        }
        Ok(())
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

    /// The `attidentity` of a column: `a`, `d`, or 0 for a column that is not an identity column.
    pub fn set_identity(&mut self, relation: u32, column: i16, identity: u8) -> Result<()> {
        self.column_mut(relation, column)?.identity = identity;
        Ok(())
    }

    /// `ATExecSetStatistics`: `attstattarget` of a column, or `None` for the default.
    pub fn set_stat_target(
        &mut self,
        relation: u32,
        column: i16,
        target: Option<i16>,
    ) -> Result<()> {
        self.column_mut(relation, column)?.stat_target = target;
        Ok(())
    }

    /// `ATExecSetStorage` and `SetIndexStorageProperties`: `attstorage` of a column and of each index column that is the column.
    pub fn set_storage(&mut self, relation: u32, column: i16, storage: u8) -> Result<()> {
        self.column_mut(relation, column)?.storage = storage;
        for (index, number) in self.index_columns_of(relation, column) {
            self.column_mut(index, number)?.storage = storage;
        }
        Ok(())
    }

    /// `ATExecSetCompression` and `SetIndexStorageProperties`: `attcompression` of a column and of each index column that is the column.
    pub fn set_compression(&mut self, relation: u32, column: i16, compression: u8) -> Result<()> {
        self.column_mut(relation, column)?.compression = compression;
        for (index, number) in self.index_columns_of(relation, column) {
            self.column_mut(index, number)?.compression = compression;
        }
        Ok(())
    }

    /// `ATExecSetOptions`: `attoptions` of a column.
    pub fn set_column_options(
        &mut self,
        relation: u32,
        column: i16,
        options: Vec<String>,
    ) -> Result<()> {
        self.column_mut(relation, column)?.options = options;
        Ok(())
    }

    /// The OID and the column number of each index column that is the column of the table, as `SetIndexStorageProperties` finds them.
    fn index_columns_of(&self, relation: u32, column: i16) -> Vec<(u32, i16)> {
        let mut found = Vec::new();
        for rel in self.relations.values() {
            let Some(info) = rel.index.as_ref().filter(|i| i.table == relation) else { continue };
            for (i, &key) in info.keys.iter().enumerate() {
                if key == column
                    && let Ok(number) = i16::try_from(i + 1)
                {
                    found.push((rel.oid, number));
                }
            }
        }
        found
    }

    /// `RemoveAttrDefault`: removes the default of a column with its rows of `pg_depend`. A column with no default is not an error.
    pub fn remove_default(&mut self, relation: u32, column: i16) {
        let Some(oid) = self.default_of(relation, column).map(|d| d.oid) else { return };
        self.defaults.remove(&oid);
        if let Ok(column) = self.column_mut(relation, column) {
            column.has_default = false;
        }
        let me = ObjRef::new(PG_ATTRDEF, oid);
        self.depends.retain(|d| d.object != me);
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
            local: true,
            inhcount: 0,
            parent: 0,
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
        let unlogged = self.relations.get(&new.table).is_some_and(|r| r.unlogged);
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
            pages: new.pages,
            clustered: false,
            replica_identity: false,
            valid: true,
            parent: 0,
        };
        let table_columns = self.relations.get(&new.table).map_or(&[][..], |r| &r.columns[..]);
        let partitioned =
            self.relations.get(&new.table).is_some_and(|r| r.kind == RelKind::Partitioned);
        let columns = new
            .columns
            .iter()
            .zip(column_names)
            .map(|(c, name)| {
                let mut column = Column::new(name, c.ty, c.typmod, c.collation);
                let from = usize::try_from(c.key).ok().and_then(|k| k.checked_sub(1));
                if let Some(from) = from.and_then(|k| table_columns.get(k)) {
                    column.ndims = from.ndims;
                    column.storage = from.storage;
                    column.compression = from.compression;
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
                kind: if partitioned { RelKind::PartitionedIndex } else { RelKind::Index },
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
                replica_identity: 0,
                partition_key: None,
                partition_of: None,
                inherits: Vec::new(),
                of_type: 0,
                unlogged,
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
            // `index_constraint_create`: a deferrable constraint gets the trigger that checks its keys again after the statement or at the commit.
            if new.deferrable {
                let mut trigger = Trigger {
                    relation: new.table,
                    constraint_index: oid,
                    constraint: referenced.oid,
                    function: UNIQUE_KEY_RECHECK,
                    kind: TRIGGER_ROW | TRIGGER_INSERT | TRIGGER_UPDATE,
                    deferrable: true,
                    deferred: new.deferred,
                    enabled: b'O',
                    internal: true,
                    ..Trigger::default()
                };
                let prefix = if kind == ConKind::Primary {
                    "PK_ConstraintTrigger"
                } else {
                    "Unique_ConstraintTrigger"
                };
                self.add_trigger(&mut trigger, prefix);
                self.relation_mut(new.table)?.has_triggers = true;
            }
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

    /// `ATAddForeignKeyConstraint`: a foreign key with its four triggers. Without a name, the name is `table_columns_fkey`.
    pub fn add_foreign_key(&mut self, new: NewForeignKey) -> Result<u32> {
        let oid = self.add_fk_constraint(new, 0, FkSide::Both)?;
        self.add_fk_action_triggers(oid, None)?;
        self.add_fk_check_triggers(oid, None)?;
        Ok(oid)
    }

    /// `addFkConstraint`: the row of `pg_constraint` of a foreign key, with no triggers. A constraint of the statement has no `parent`. A clone of the constraint `parent` serves one side: a clone for a partition of the referenced table depends on `parent` as a part of it, and a clone for a partition of the referencing table is a partition of `parent`. A clone keeps the name of `parent`, unless the table has a constraint with that name, and then the name gets `_1`, `_2` and so on. The constraint is `NO INHERIT` unless its table is partitioned.
    pub fn add_fk_constraint(
        &mut self,
        new: NewForeignKey,
        parent: u32,
        side: FkSide,
    ) -> Result<u32> {
        let (relname, namespace) = self.relation_facts(new.table)?;
        let name = match &new.name {
            Some(name) if parent != 0 => {
                if self.constraints_of(new.table).any(|c| c.name == *name) {
                    self.choose_constraint_name(name, None, "", namespace)
                } else {
                    name.clone()
                }
            }
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
        let partitioned =
            self.relations.get(&new.table).is_some_and(|r| r.kind == RelKind::Partitioned);
        let mut constraint =
            self.new_constraint(name, namespace, ConKind::Foreign, new.table, new.keys);
        constraint.index = new.index;
        constraint.no_inherit = !partitioned && parent == 0;
        constraint.deferrable = new.deferrable;
        constraint.deferred = new.deferred;
        if parent != 0 {
            constraint.parent = parent;
            constraint.local = false;
            constraint.inhcount = 1;
        }
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
        match side {
            _ if parent == 0 => {}
            FkSide::Referenced => {
                self.depend(me, ObjRef::new(PG_CONSTRAINT, parent), DepKind::Internal);
            }
            FkSide::Both | FkSide::Referencing => {
                self.depend(me, ObjRef::new(PG_CONSTRAINT, parent), DepKind::PartitionPri);
                self.depend(me, ObjRef::new(PG_CLASS, new.table), DepKind::PartitionSec);
            }
        }
        Ok(oid)
    }

    /// `createForeignKeyActionTriggers`: the triggers of the actions of a foreign key on its referenced table, for a delete and for an update, in this order. Only `NO ACTION` can wait to the end of the transaction. The triggers of a clone are the clones of `parents`, the triggers of its parent constraint.
    pub fn add_fk_action_triggers(
        &mut self,
        constraint: u32,
        parents: Option<(u32, u32)>,
    ) -> Result<(u32, u32)> {
        let (mut trigger, foreign) = self.fk_trigger(constraint)?;
        std::mem::swap(&mut trigger.relation, &mut trigger.constraint_relation);
        let (delete, update) = (foreign.delete, foreign.update);
        let (deferrable, deferred) = (trigger.deferrable, trigger.deferred);
        let mut oids = [0; 2];
        for (at, (event, action, offset)) in
            [(TRIGGER_DELETE, delete, 0), (TRIGGER_UPDATE, update, 1)].into_iter().enumerate()
        {
            let function = match action {
                'r' => 4,
                'c' => 2,
                'n' => 6,
                'd' => 8,
                _ => 10,
            };
            let no_action = function == 10;
            trigger.function = RI_FKEY_CHECK_INS + function + offset;
            trigger.kind = TRIGGER_ROW | event;
            trigger.deferrable = no_action && deferrable;
            trigger.deferred = no_action && deferred;
            trigger.parent = parents.map_or(0, |p| if at == 0 { p.0 } else { p.1 });
            self.add_trigger(&mut trigger, "RI_ConstraintTrigger_a");
            oids[at] = trigger.oid;
        }
        self.relation_mut(trigger.relation)?.has_triggers = true;
        Ok((oids[0], oids[1]))
    }

    /// `createForeignKeyCheckTriggers`: the checks of a foreign key on its referencing table, for an insert and for an update, in this order. The triggers of a clone are the clones of `parents`, the triggers of its parent constraint.
    pub fn add_fk_check_triggers(
        &mut self,
        constraint: u32,
        parents: Option<(u32, u32)>,
    ) -> Result<(u32, u32)> {
        let (mut trigger, _) = self.fk_trigger(constraint)?;
        let mut oids = [0; 2];
        for (at, (event, offset)) in
            [(TRIGGER_INSERT, 0), (TRIGGER_UPDATE, 1)].into_iter().enumerate()
        {
            trigger.function = RI_FKEY_CHECK_INS + offset;
            trigger.kind = TRIGGER_ROW | event;
            trigger.parent = parents.map_or(0, |p| if at == 0 { p.0 } else { p.1 });
            self.add_trigger(&mut trigger, "RI_ConstraintTrigger_c");
            oids[at] = trigger.oid;
        }
        self.relation_mut(trigger.relation)?.has_triggers = true;
        Ok((oids[0], oids[1]))
    }

    /// The facts that the triggers of a foreign key share, with the referencing table as the table of the trigger.
    fn fk_trigger(&self, constraint: u32) -> Result<(Trigger, ForeignKey)> {
        let con =
            self.constraints.get(&constraint).ok_or_else(|| no_object("constraint", constraint))?;
        let Some(foreign) = con.foreign.clone() else {
            return Err(Error::internal(format!("constraint {constraint} is not a foreign key")));
        };
        let trigger = Trigger {
            relation: con.relation,
            constraint_relation: foreign.table,
            constraint_index: con.index,
            constraint,
            deferrable: con.deferrable,
            deferred: con.deferred,
            enabled: b'O',
            internal: true,
            ..Trigger::default()
        };
        Ok((trigger, foreign))
    }

    /// `TriggerSetParentTrigger`: the trigger becomes a clone of the trigger `parent`, with the partition dependencies on `parent` and on its table.
    pub fn set_trigger_parent(&mut self, trigger: u32, parent: u32) {
        let Some(found) = self.triggers.get_mut(&trigger) else { return };
        found.parent = parent;
        let table = found.relation;
        let me = ObjRef::new(PG_TRIGGER, trigger);
        self.depend(me, ObjRef::new(PG_TRIGGER, parent), DepKind::PartitionPri);
        self.depend(me, ObjRef::new(PG_CLASS, table), DepKind::PartitionSec);
    }

    /// `deleteDependencyRecordsFor`: the object loses all its rows of `pg_depend`, so that it can go alone.
    pub fn drop_depends_of(&mut self, object: ObjRef) {
        self.depends.retain(|d| d.object != object);
    }

    /// `CreateTrigger` for an internal trigger of a constraint: a new OID, the name `prefix_OID`, and an internal dependency on the constraint. The dependency on the function is not recorded, because the function is pinned.
    fn add_trigger(&mut self, trigger: &mut Trigger, prefix: &str) {
        trigger.oid = self.new_oid();
        trigger.name = format!("{prefix}_{}", trigger.oid);
        self.triggers.insert(trigger.oid, trigger.clone());
        let me = ObjRef::new(PG_TRIGGER, trigger.oid);
        self.depend(me, ObjRef::new(PG_CONSTRAINT, trigger.constraint), DepKind::Internal);
        if trigger.parent != 0 {
            self.depend(me, ObjRef::new(PG_TRIGGER, trigger.parent), DepKind::PartitionPri);
            self.depend(me, ObjRef::new(PG_CLASS, trigger.relation), DepKind::PartitionSec);
        }
    }

    /// `create_toast_table`: the TOAST table of a table and its index, in the schema `pg_toast`, or in `pg_toast_temp_N` for a table of the temporary schema `pg_temp_N`. The TOAST table gets an OID, then its index. They have no row type.
    pub fn create_toast(&mut self, table: u32) -> Result<u32> {
        let rel = self.relations.get(&table).ok_or_else(|| no_object("relation", table))?;
        let (owner, unlogged) = (rel.owner, rel.unlogged);
        let namespace = self.toast_namespace(rel.namespace);
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
                namespace,
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
                replica_identity: 0,
                partition_key: None,
                partition_of: None,
                inherits: Vec::new(),
                of_type: 0,
                unlogged,
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
            pages: 1,
            clustered: false,
            replica_identity: false,
            valid: true,
            parent: 0,
        };
        self.relations.insert(
            index,
            Relation {
                oid: index,
                name: format!("{name}_index"),
                namespace,
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
                replica_identity: 0,
                partition_key: None,
                partition_of: None,
                inherits: Vec::new(),
                of_type: 0,
                unlogged,
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

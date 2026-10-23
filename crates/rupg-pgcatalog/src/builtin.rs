//! Typed rows of the built-in types, functions, operators, casts and operator classes, with indexes by OID and by name.
//!
//! The analyzer resolves names, operators, functions and casts with these rows, as `parse_oper.c`, `parse_func.c` and `parse_coerce.c` do with the syscache of PostgreSQL. It finds the sort and equality operators of a type with the rows of `pg_opclass` and `pg_amop`, as `typcache.c` does. The rows are read once from the static batches of the catalogs, so they hold the values of the pin and nothing else.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError, RwLock};

use crate::{Batch, Catalog, Values, catalog};

/// A row of `pg_type`.
#[derive(Clone, Copy, Debug)]
pub struct TypeRow {
    pub oid: u32,
    pub name: &'static str,
    pub namespace: u32,
    /// `typlen`: the length of a fixed-length type, -1 for a varlena and -2 for a C string.
    pub len: i16,
    pub by_val: bool,
    /// `typtype`: `b` base, `c` composite, `d` domain, `e` enum, `p` pseudo, `r` range or `m` multirange.
    pub kind: u8,
    /// `typcategory`, such as `N` for the numeric types and `S` for the string types.
    pub category: u8,
    pub preferred: bool,
    pub delim: u8,
    /// `typrelid`: the table of a composite type, or 0.
    pub relid: u32,
    /// `typsubscript`: the subscript handler, or 0 for a type that has no subscripts.
    pub subscript: u32,
    /// `typelem`: the element type of an array type, or 0.
    pub elem: u32,
    /// `typarray`: the array type of this type, or 0.
    pub array: u32,
    pub input: u32,
    pub output: u32,
    pub receive: u32,
    pub send: u32,
    pub modin: u32,
    pub modout: u32,
    pub align: u8,
    /// `typstorage`: `p`, `e`, `m` or `x`.
    pub storage: u8,
    /// `typbasetype`: the base type of a domain, or 0.
    pub base: u32,
    pub typmod: i32,
    pub collation: u32,
}

/// A row of `pg_proc`.
#[derive(Clone, Copy, Debug)]
pub struct ProcRow {
    pub oid: u32,
    pub name: &'static str,
    pub namespace: u32,
    pub lang: u32,
    pub variadic: u32,
    /// `prokind`: `f` function, `p` procedure, `a` aggregate or `w` window function.
    pub kind: u8,
    pub strict: bool,
    pub retset: bool,
    /// `provolatile`: `i` immutable, `s` stable or `v` volatile.
    pub volatile: u8,
    pub nargs: i16,
    pub nargdefaults: i16,
    pub rettype: u32,
    /// `proargtypes`: the types of the input arguments.
    pub argtypes: &'static [u32],
    /// `proallargtypes`: the types of all the arguments, with the output arguments, or `None` if all are inputs.
    pub allargtypes: Option<&'static [u32]>,
    /// `proargmodes`, or `None` if all arguments are inputs.
    pub argmodes: Option<&'static [u8]>,
    /// `proargnames`, or `None` if no argument has a name.
    pub argnames: Option<&'static [&'static str]>,
    /// `proargdefaults` as the text of the node tree, or `None`. A function that `CREATE FUNCTION` makes has the stored form of `rupg-analyze`.
    pub argdefaults: Option<&'static str>,
    /// `prosrc`: for an internal function, the name of the C function, which is the name of its kernel.
    pub src: &'static str,
    /// `prosqlbody`: for a function in SQL with a body in SQL, the text of the statement that made it.
    pub sqlbody: Option<&'static str>,
}

/// A row of `pg_operator`.
#[derive(Debug)]
pub struct OperatorRow {
    pub oid: u32,
    pub name: &'static str,
    pub namespace: u32,
    /// `oprkind`: `b` for an infix operator or `l` for a prefix operator.
    pub kind: u8,
    pub can_merge: bool,
    pub can_hash: bool,
    /// The type of the left input, or 0 for a prefix operator.
    pub left: u32,
    pub right: u32,
    pub result: u32,
    pub commutator: u32,
    pub negator: u32,
    /// `oprcode`: the function that implements the operator.
    pub code: u32,
}

/// A row of `pg_cast`.
#[derive(Debug)]
pub struct CastRow {
    pub oid: u32,
    pub source: u32,
    pub target: u32,
    /// The function of the cast, or 0 for a cast without a function.
    pub func: u32,
    /// `castcontext`: `i` implicit, `a` assignment or `e` explicit.
    pub context: u8,
    /// `castmethod`: `f` function, `i` the input and output functions, or `b` binary compatible.
    pub method: u8,
}

/// A row of `pg_opclass`.
#[derive(Debug)]
pub struct OpclassRow {
    pub oid: u32,
    /// `opcmethod`: the access method, such as 403 for btree and 405 for hash.
    pub method: u32,
    pub name: &'static str,
    pub family: u32,
    /// `opcintype`: the type that the class indexes.
    pub intype: u32,
    /// `opcdefault`: true for the default class of the type and the method.
    pub default: bool,
}

/// A row of `pg_amop`.
#[derive(Debug)]
pub struct AmopRow {
    pub oid: u32,
    pub family: u32,
    pub left: u32,
    pub right: u32,
    pub strategy: i16,
    /// `amoppurpose`: `s` for search or `o` for ordering.
    pub purpose: u8,
    pub operator: u32,
    pub method: u32,
}

/// A row of `pg_amproc`.
#[derive(Debug)]
pub struct AmprocRow {
    pub oid: u32,
    pub family: u32,
    pub left: u32,
    pub right: u32,
    /// `amprocnum`: the number of the support function in the family, as `BTINRANGE_PROC` is 3 for `in_range`.
    pub number: i16,
    /// `amproc`: the function.
    pub proc: u32,
}

/// A row of `pg_aggregate`.
#[derive(Debug)]
pub struct AggregateRow {
    /// `aggfnoid`: the function of the aggregate in `pg_proc`.
    pub fnoid: u32,
    /// `aggkind`: `n` for a normal aggregate, `o` for an ordered-set aggregate or `h` for a hypothetical-set aggregate.
    pub kind: u8,
    /// `aggnumdirectargs`: the number of direct arguments of an ordered-set aggregate.
    pub ndirect: i16,
    /// `aggtransfn`: the transition function.
    pub transfn: u32,
    /// `aggfinalfn`: the final function, or 0.
    pub finalfn: u32,
    /// `aggfinalextra`: true when the final function gets a null for each argument of the aggregate after the state.
    pub finalextra: bool,
    /// `aggsortop`: the sort operator of `min` and `max`, or 0.
    pub sortop: u32,
    /// `aggtranstype`: the type of the state.
    pub transtype: u32,
    /// `agginitval`: the text of the first state, or `None` when the first state is null.
    pub initval: Option<&'static str>,
}

/// A built-in object that has a name, of a kind that an OID alias type or a function such as `pg_opclass_is_visible` reads.
#[derive(Debug)]
pub struct NamedRow {
    pub oid: u32,
    pub name: &'static str,
    /// The schema, or 0 for a kind that has no schema.
    pub namespace: u32,
    /// `collencoding` of a collation, -1 for a collation of any encoding. It is -1 for the other kinds.
    pub encoding: i32,
    /// The access method of an operator class or an operator family, or 0 for the other kinds.
    pub method: u32,
}

/// The kinds of [`NamedRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Named {
    /// `pg_class`.
    Class,
    /// `pg_namespace`.
    Namespace,
    /// `pg_authid`.
    Role,
    /// `pg_database`.
    Database,
    /// `pg_collation`.
    Collation,
    /// `pg_ts_config`.
    Config,
    /// `pg_ts_dict`.
    Dictionary,
    /// `pg_ts_parser`.
    Parser,
    /// `pg_ts_template`.
    Template,
    /// `pg_opclass`.
    Opclass,
    /// `pg_opfamily`.
    Opfamily,
    /// `pg_conversion`.
    Conversion,
}

impl Named {
    /// The catalog, the column of the name, the column of the schema and the column of the access method. A column that the kind does not have is `""`.
    fn columns(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Named::Class => ("pg_class", "relname", "relnamespace", ""),
            Named::Namespace => ("pg_namespace", "nspname", "", ""),
            Named::Role => ("pg_authid", "rolname", "", ""),
            Named::Database => ("pg_database", "datname", "", ""),
            Named::Collation => ("pg_collation", "collname", "collnamespace", ""),
            Named::Config => ("pg_ts_config", "cfgname", "cfgnamespace", ""),
            Named::Dictionary => ("pg_ts_dict", "dictname", "dictnamespace", ""),
            Named::Parser => ("pg_ts_parser", "prsname", "prsnamespace", ""),
            Named::Template => ("pg_ts_template", "tmplname", "tmplnamespace", ""),
            Named::Opclass => ("pg_opclass", "opcname", "opcnamespace", "opcmethod"),
            Named::Opfamily => ("pg_opfamily", "opfname", "opfnamespace", "opfmethod"),
            Named::Conversion => ("pg_conversion", "conname", "connamespace", ""),
        }
    }

    const ALL: [Named; 12] = [
        Named::Class,
        Named::Namespace,
        Named::Role,
        Named::Database,
        Named::Collation,
        Named::Config,
        Named::Dictionary,
        Named::Parser,
        Named::Template,
        Named::Opclass,
        Named::Opfamily,
        Named::Conversion,
    ];
}

/// A row of `pg_class`, with the columns that the privilege checks and the functions of the files of the relations read.
#[derive(Debug)]
pub struct ClassRow {
    pub oid: u32,
    pub name: &'static str,
    pub namespace: u32,
    /// `relkind`: `r` table, `i` index, `t` toast table, `v` view, `S` sequence and the others.
    pub kind: u8,
    /// `relnatts`: the number of user columns.
    pub natts: i16,
    pub owner: u32,
    /// `relacl`, or `None` for the default privileges of the owner.
    pub acl: Option<&'static [&'static str]>,
    /// `relfilenode`, or 0 for a mapped catalog and a relation with no storage.
    pub filenode: u32,
    /// `relisshared`.
    pub shared: bool,
}

/// A row of `pg_attribute`, with the columns that the privilege checks read.
#[derive(Debug)]
pub struct AttributeRow {
    pub name: &'static str,
    pub num: i16,
    pub dropped: bool,
    /// `attacl`, or `None` when the column has no privileges of its own.
    pub acl: Option<&'static [&'static str]>,
}

/// A built-in object of a kind that has an owner and privileges, other than a relation.
#[derive(Debug)]
pub struct OwnedRow {
    pub oid: u32,
    /// The name, or `""` for a large object.
    pub name: &'static str,
    pub owner: u32,
    /// The `aclitem[]` column, or `None` for the default privileges of the owner.
    pub acl: Option<&'static [&'static str]>,
}

/// The kinds of [`OwnedRow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Owned {
    /// `pg_namespace`.
    Namespace,
    /// `pg_database`.
    Database,
    /// `pg_proc`.
    Function,
    /// `pg_type`.
    Type,
    /// `pg_language`.
    Language,
    /// `pg_tablespace`.
    Tablespace,
    /// `pg_foreign_data_wrapper`.
    ForeignDataWrapper,
    /// `pg_foreign_server`.
    ForeignServer,
    /// `pg_largeobject_metadata`.
    LargeObject,
}

impl Owned {
    /// The catalog, the column of the name, the column of the owner and the column of the privileges. A column that the kind does not have is `""`.
    fn columns(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Owned::Namespace => ("pg_namespace", "nspname", "nspowner", "nspacl"),
            Owned::Database => ("pg_database", "datname", "datdba", "datacl"),
            Owned::Function => ("pg_proc", "proname", "proowner", "proacl"),
            Owned::Type => ("pg_type", "typname", "typowner", "typacl"),
            Owned::Language => ("pg_language", "lanname", "lanowner", "lanacl"),
            Owned::Tablespace => ("pg_tablespace", "spcname", "spcowner", "spcacl"),
            Owned::ForeignDataWrapper => {
                ("pg_foreign_data_wrapper", "fdwname", "fdwowner", "fdwacl")
            }
            Owned::ForeignServer => ("pg_foreign_server", "srvname", "srvowner", "srvacl"),
            Owned::LargeObject => ("pg_largeobject_metadata", "", "lomowner", "lomacl"),
        }
    }

    const ALL: [Owned; 9] = [
        Owned::Namespace,
        Owned::Database,
        Owned::Function,
        Owned::Type,
        Owned::Language,
        Owned::Tablespace,
        Owned::ForeignDataWrapper,
        Owned::ForeignServer,
        Owned::LargeObject,
    ];
}

/// A row of `pg_authid`, with the columns that the privilege checks read.
#[derive(Debug)]
pub struct RoleRow {
    pub oid: u32,
    pub name: &'static str,
    pub attrs: crate::roles::Attrs,
}

/// A row of `pg_auth_members`: `member` is a member of `role`.
#[derive(Clone, Copy, Debug)]
pub struct MemberRow {
    pub role: u32,
    pub member: u32,
    /// `admin_option`: the member can grant the membership to other roles.
    pub admin: bool,
    /// `inherit_option`: the member has the privileges of the role.
    pub inherit: bool,
    /// `set_option`: the member can `SET ROLE` to the role.
    pub set: bool,
}

/// A row of `pg_description`, or of `pg_shdescription` with `objsubid` 0.
#[derive(Debug)]
pub struct DescriptionRow {
    pub objoid: u32,
    pub classoid: u32,
    pub objsubid: i32,
    pub description: &'static str,
}

struct Builtin {
    types: Vec<TypeRow>,
    procs: Vec<ProcRow>,
    operators: Vec<OperatorRow>,
    casts: Vec<CastRow>,
    opclasses: Vec<OpclassRow>,
    amops: Vec<AmopRow>,
    type_oid: HashMap<u32, usize>,
    type_name: HashMap<&'static str, Vec<usize>>,
    proc_oid: HashMap<u32, usize>,
    proc_name: HashMap<&'static str, Vec<usize>>,
    operator_oid: HashMap<u32, usize>,
    operator_name: HashMap<&'static str, Vec<usize>>,
    cast_pair: HashMap<(u32, u32), usize>,
    amop_member: HashMap<(u32, u32, u32, i16), usize>,
    amop_operator: HashMap<u32, Vec<usize>>,
    /// The rows of `pg_amproc` by the family and the left type.
    amprocs: HashMap<(u32, u32), Vec<AmprocRow>>,
    aggregates: Vec<AggregateRow>,
    aggregate_fn: HashMap<u32, usize>,
    primary_keys: HashMap<u32, Vec<i16>>,
    named: HashMap<Named, Vec<NamedRow>>,
    descriptions: Vec<DescriptionRow>,
    description_key: HashMap<(u32, u32, i32), usize>,
    shared_descriptions: Vec<DescriptionRow>,
    classes: Vec<ClassRow>,
    class_oid: HashMap<u32, usize>,
    attributes: HashMap<u32, Vec<AttributeRow>>,
    owned: HashMap<Owned, Vec<OwnedRow>>,
    owned_oid: HashMap<(Owned, u32), usize>,
    roles: Vec<RoleRow>,
    members: Vec<MemberRow>,
    multirange_range: HashMap<u32, u32>,
    /// The subtype and the multirange type of each range type.
    range_types: HashMap<u32, (u32, u32)>,
}

static BUILTIN: LazyLock<Builtin> = LazyLock::new(Builtin::load);

/// The columns of one catalog, read by name.
struct Rows {
    catalog: &'static Catalog,
}

impl Rows {
    fn new(name: &str) -> Rows {
        Rows { catalog: catalog(name).expect("a catalog of the pin") }
    }

    fn batch(&self, column: &str) -> &'static Batch {
        let at = self.catalog.column(column).expect("a column of the catalog");
        &self.catalog.rows[at]
    }

    fn oid(&self, column: &str) -> &'static [u32] {
        match self.batch(column).values {
            Values::Oid(v) => v,
            _ => panic!("{column} is not an OID column"),
        }
    }

    fn text(&self, column: &str) -> &'static [&'static str] {
        match self.batch(column).values {
            Values::Text(v) => v,
            _ => panic!("{column} is not a text column"),
        }
    }

    fn bool(&self, column: &str) -> &'static [bool] {
        match self.batch(column).values {
            Values::Bool(v) => v,
            _ => panic!("{column} is not a bool column"),
        }
    }

    fn char(&self, column: &str) -> &'static [u8] {
        match self.batch(column).values {
            Values::Char(v) => v,
            _ => panic!("{column} is not a char column"),
        }
    }

    fn int2(&self, column: &str) -> &'static [i16] {
        match self.batch(column).values {
            Values::Int2(v) => v,
            _ => panic!("{column} is not an int2 column"),
        }
    }

    fn int4(&self, column: &str) -> &'static [i32] {
        match self.batch(column).values {
            Values::Int4(v) => v,
            _ => panic!("{column} is not an int4 column"),
        }
    }

    /// A column whose values are all null has no values. A null value reads as `None`.
    fn optional<T: Copy + 'static>(
        &self,
        column: &str,
        row: usize,
        values: impl Fn(&'static Values) -> Option<&'static [T]>,
    ) -> Option<T> {
        let batch = self.batch(column);
        if batch.is_null(row) {
            return None;
        }
        values(&batch.values).map(|v| v[row])
    }
}

impl Builtin {
    fn load() -> Builtin {
        let types = load_types();
        let procs = load_procs();
        let operators = load_operators();
        let casts = load_casts();
        let opclasses = load_opclasses();
        let amops = load_amops();
        let aggregates = load_aggregates();
        let descriptions = load_descriptions("pg_description");
        let classes = load_classes();
        let owned: HashMap<Owned, Vec<OwnedRow>> =
            Owned::ALL.into_iter().map(|kind| (kind, load_owned(kind))).collect();
        let mut amop_member = HashMap::new();
        let mut amop_operator: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, a) in amops.iter().enumerate() {
            if a.purpose == b's' {
                amop_member.insert((a.family, a.left, a.right, a.strategy), i);
            }
            amop_operator.entry(a.operator).or_default().push(i);
        }
        let mut proc_name: HashMap<&'static str, Vec<usize>> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            proc_name.entry(p.name).or_default().push(i);
        }
        let mut type_name: HashMap<&'static str, Vec<usize>> = HashMap::new();
        for (i, t) in types.iter().enumerate() {
            type_name.entry(t.name).or_default().push(i);
        }
        let mut operator_name: HashMap<&'static str, Vec<usize>> = HashMap::new();
        for (i, o) in operators.iter().enumerate() {
            operator_name.entry(o.name).or_default().push(i);
        }
        Builtin {
            type_oid: types.iter().enumerate().map(|(i, t)| (t.oid, i)).collect(),
            type_name,
            proc_oid: procs.iter().enumerate().map(|(i, p)| (p.oid, i)).collect(),
            proc_name,
            operator_oid: operators.iter().enumerate().map(|(i, o)| (o.oid, i)).collect(),
            operator_name,
            cast_pair: casts.iter().enumerate().map(|(i, c)| ((c.source, c.target), i)).collect(),
            amop_member,
            amop_operator,
            amprocs: load_amprocs(),
            aggregate_fn: aggregates.iter().enumerate().map(|(i, a)| (a.fnoid, i)).collect(),
            aggregates,
            primary_keys: load_primary_keys(),
            named: Named::ALL.into_iter().map(|kind| (kind, load_named(kind))).collect(),
            description_key: descriptions
                .iter()
                .enumerate()
                .map(|(i, d)| ((d.objoid, d.classoid, d.objsubid), i))
                .collect(),
            descriptions,
            shared_descriptions: load_descriptions("pg_shdescription"),
            class_oid: classes.iter().enumerate().map(|(i, c)| (c.oid, i)).collect(),
            classes,
            attributes: load_attributes(),
            owned_oid: owned
                .iter()
                .flat_map(|(kind, rows)| rows.iter().enumerate().map(|(i, r)| ((*kind, r.oid), i)))
                .collect(),
            owned,
            roles: load_roles(),
            members: load_members(),
            multirange_range: load_multirange_ranges(),
            range_types: load_range_types(),
            types,
            procs,
            operators,
            casts,
            opclasses,
            amops,
        }
    }
}

fn load_types() -> Vec<TypeRow> {
    let r = Rows::new("pg_type");
    let (oid, name, namespace) = (r.oid("oid"), r.text("typname"), r.oid("typnamespace"));
    let (len, by_val, kind) = (r.int2("typlen"), r.bool("typbyval"), r.char("typtype"));
    let (category, preferred) = (r.char("typcategory"), r.bool("typispreferred"));
    let (delim, relid, subscript) = (r.char("typdelim"), r.oid("typrelid"), r.oid("typsubscript"));
    let (elem, array) = (r.oid("typelem"), r.oid("typarray"));
    let (input, output) = (r.oid("typinput"), r.oid("typoutput"));
    let (receive, send) = (r.oid("typreceive"), r.oid("typsend"));
    let (modin, modout, align) = (r.oid("typmodin"), r.oid("typmodout"), r.char("typalign"));
    let storage = r.char("typstorage");
    let (base, typmod, collation) =
        (r.oid("typbasetype"), r.int4("typtypmod"), r.oid("typcollation"));
    (0..r.catalog.len)
        .map(|i| TypeRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace[i],
            len: len[i],
            by_val: by_val[i],
            kind: kind[i],
            category: category[i],
            preferred: preferred[i],
            delim: delim[i],
            relid: relid[i],
            subscript: subscript[i],
            elem: elem[i],
            array: array[i],
            input: input[i],
            output: output[i],
            receive: receive[i],
            send: send[i],
            modin: modin[i],
            modout: modout[i],
            align: align[i],
            storage: storage[i],
            base: base[i],
            typmod: typmod[i],
            collation: collation[i],
        })
        .collect()
}

fn load_procs() -> Vec<ProcRow> {
    let r = Rows::new("pg_proc");
    let (oid, name, namespace, lang) =
        (r.oid("oid"), r.text("proname"), r.oid("pronamespace"), r.oid("prolang"));
    let (variadic, kind, strict) = (r.oid("provariadic"), r.char("prokind"), r.bool("proisstrict"));
    let (retset, volatile) = (r.bool("proretset"), r.char("provolatile"));
    let (nargs, nargdefaults, rettype) =
        (r.int2("pronargs"), r.int2("pronargdefaults"), r.oid("prorettype"));
    let argtypes = match r.batch("proargtypes").values {
        Values::OidVector(v) => v,
        _ => panic!("proargtypes is not an oidvector column"),
    };
    let src = r.text("prosrc");
    (0..r.catalog.len)
        .map(|i| ProcRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace[i],
            lang: lang[i],
            variadic: variadic[i],
            kind: kind[i],
            strict: strict[i],
            retset: retset[i],
            volatile: volatile[i],
            nargs: nargs[i],
            nargdefaults: nargdefaults[i],
            rettype: rettype[i],
            argtypes: argtypes[i],
            allargtypes: r.optional("proallargtypes", i, |v| match v {
                Values::OidArray(v) => Some(*v),
                _ => None,
            }),
            argmodes: r.optional("proargmodes", i, |v| match v {
                Values::CharArray(v) => Some(*v),
                _ => None,
            }),
            argnames: r.optional("proargnames", i, |v| match v {
                Values::TextArray(v) => Some(*v),
                _ => None,
            }),
            argdefaults: r.optional("proargdefaults", i, |v| match v {
                Values::Text(v) => Some(*v),
                _ => None,
            }),
            src: src[i],
            sqlbody: r.optional("prosqlbody", i, |v| match v {
                Values::Text(v) => Some(*v),
                _ => None,
            }),
        })
        .collect()
}

fn load_operators() -> Vec<OperatorRow> {
    let r = Rows::new("pg_operator");
    let (oid, name, namespace, kind) =
        (r.oid("oid"), r.text("oprname"), r.oid("oprnamespace"), r.char("oprkind"));
    let (can_merge, can_hash) = (r.bool("oprcanmerge"), r.bool("oprcanhash"));
    let (left, right, result) = (r.oid("oprleft"), r.oid("oprright"), r.oid("oprresult"));
    let (commutator, negator, code) = (r.oid("oprcom"), r.oid("oprnegate"), r.oid("oprcode"));
    (0..r.catalog.len)
        .map(|i| OperatorRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace[i],
            kind: kind[i],
            can_merge: can_merge[i],
            can_hash: can_hash[i],
            left: left[i],
            right: right[i],
            result: result[i],
            commutator: commutator[i],
            negator: negator[i],
            code: code[i],
        })
        .collect()
}

fn load_casts() -> Vec<CastRow> {
    let r = Rows::new("pg_cast");
    let (oid, source, target, func) =
        (r.oid("oid"), r.oid("castsource"), r.oid("casttarget"), r.oid("castfunc"));
    let (context, method) = (r.char("castcontext"), r.char("castmethod"));
    (0..r.catalog.len)
        .map(|i| CastRow {
            oid: oid[i],
            source: source[i],
            target: target[i],
            func: func[i],
            context: context[i],
            method: method[i],
        })
        .collect()
}

fn load_opclasses() -> Vec<OpclassRow> {
    let r = Rows::new("pg_opclass");
    let (oid, method, name) = (r.oid("oid"), r.oid("opcmethod"), r.text("opcname"));
    let (family, intype, default) = (r.oid("opcfamily"), r.oid("opcintype"), r.bool("opcdefault"));
    (0..r.catalog.len)
        .map(|i| OpclassRow {
            oid: oid[i],
            method: method[i],
            name: name[i],
            family: family[i],
            intype: intype[i],
            default: default[i],
        })
        .collect()
}

fn load_amops() -> Vec<AmopRow> {
    let r = Rows::new("pg_amop");
    let oid = r.oid("oid");
    let (family, left, right) =
        (r.oid("amopfamily"), r.oid("amoplefttype"), r.oid("amoprighttype"));
    let (strategy, purpose) = (r.int2("amopstrategy"), r.char("amoppurpose"));
    let (operator, method) = (r.oid("amopopr"), r.oid("amopmethod"));
    (0..r.catalog.len)
        .map(|i| AmopRow {
            oid: oid[i],
            family: family[i],
            left: left[i],
            right: right[i],
            strategy: strategy[i],
            purpose: purpose[i],
            operator: operator[i],
            method: method[i],
        })
        .collect()
}

fn load_amprocs() -> HashMap<(u32, u32), Vec<AmprocRow>> {
    let r = Rows::new("pg_amproc");
    let oid = r.oid("oid");
    let (family, left, right) =
        (r.oid("amprocfamily"), r.oid("amproclefttype"), r.oid("amprocrighttype"));
    let (number, proc) = (r.int2("amprocnum"), r.oid("amproc"));
    let mut out: HashMap<(u32, u32), Vec<AmprocRow>> = HashMap::new();
    for i in 0..r.catalog.len {
        let row = AmprocRow {
            oid: oid[i],
            family: family[i],
            left: left[i],
            right: right[i],
            number: number[i],
            proc: proc[i],
        };
        out.entry((row.family, row.left)).or_default().push(row);
    }
    out
}

fn load_aggregates() -> Vec<AggregateRow> {
    let r = Rows::new("pg_aggregate");
    let (fnoid, kind, ndirect) = (r.oid("aggfnoid"), r.char("aggkind"), r.int2("aggnumdirectargs"));
    let (transfn, finalfn, finalextra) =
        (r.oid("aggtransfn"), r.oid("aggfinalfn"), r.bool("aggfinalextra"));
    let (sortop, transtype) = (r.oid("aggsortop"), r.oid("aggtranstype"));
    (0..r.catalog.len)
        .map(|i| AggregateRow {
            fnoid: fnoid[i],
            kind: kind[i],
            ndirect: ndirect[i],
            transfn: transfn[i],
            finalfn: finalfn[i],
            finalextra: finalextra[i],
            sortop: sortop[i],
            transtype: transtype[i],
            initval: r.optional("agginitval", i, |v| match v {
                Values::Text(v) => Some(*v),
                _ => None,
            }),
        })
        .collect()
}

/// The built-in objects of a kind, from its catalog.
fn load_named(kind: Named) -> Vec<NamedRow> {
    let (catalog, name, namespace, method) = kind.columns();
    let r = Rows::new(catalog);
    let (oid, name) = (r.oid("oid"), r.text(name));
    let namespace = if namespace.is_empty() { None } else { Some(r.oid(namespace)) };
    let method = if method.is_empty() { None } else { Some(r.oid(method)) };
    let encoding = (kind == Named::Collation).then(|| r.int4("collencoding"));
    (0..r.catalog.len)
        .map(|i| NamedRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace.map_or(0, |n| n[i]),
            encoding: encoding.map_or(-1, |e| e[i]),
            method: method.map_or(0, |m| m[i]),
        })
        .collect()
}

/// The rows of `pg_class`.
fn load_classes() -> Vec<ClassRow> {
    let r = Rows::new("pg_class");
    let (oid, name, namespace) = (r.oid("oid"), r.text("relname"), r.oid("relnamespace"));
    let (kind, natts, owner) = (r.char("relkind"), r.int2("relnatts"), r.oid("relowner"));
    let (filenode, shared) = (r.oid("relfilenode"), r.bool("relisshared"));
    (0..r.catalog.len)
        .map(|i| ClassRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace[i],
            kind: kind[i],
            natts: natts[i],
            owner: owner[i],
            acl: r.optional("relacl", i, text_array),
            filenode: filenode[i],
            shared: shared[i],
        })
        .collect()
}

/// The rows of `pg_attribute`, by relation, in the order of the catalog.
fn load_attributes() -> HashMap<u32, Vec<AttributeRow>> {
    let r = Rows::new("pg_attribute");
    let (relid, name, num) = (r.oid("attrelid"), r.text("attname"), r.int2("attnum"));
    let dropped = r.bool("attisdropped");
    let mut out: HashMap<u32, Vec<AttributeRow>> = HashMap::new();
    for i in 0..r.catalog.len {
        out.entry(relid[i]).or_default().push(AttributeRow {
            name: name[i],
            num: num[i],
            dropped: dropped[i],
            acl: r.optional("attacl", i, text_array),
        });
    }
    out
}

/// The objects of a kind that has an owner, from its catalog. A catalog that has no static rows gives none.
fn load_owned(kind: Owned) -> Vec<OwnedRow> {
    let (catalog, name, owner, acl) = kind.columns();
    let r = Rows::new(catalog);
    if r.catalog.len == 0 {
        return Vec::new();
    }
    let (oid, owners) = (r.oid("oid"), r.oid(owner));
    let names = if name.is_empty() { None } else { Some(r.text(name)) };
    (0..r.catalog.len)
        .map(|i| OwnedRow {
            oid: oid[i],
            name: names.map_or("", |n| n[i]),
            owner: owners[i],
            acl: r.optional(acl, i, text_array),
        })
        .collect()
}

/// The rows of `pg_authid`.
fn load_roles() -> Vec<RoleRow> {
    let r = Rows::new("pg_authid");
    let (oid, name, superuser) = (r.oid("oid"), r.text("rolname"), r.bool("rolsuper"));
    let (inherit, createrole) = (r.bool("rolinherit"), r.bool("rolcreaterole"));
    let (createdb, login) = (r.bool("rolcreatedb"), r.bool("rolcanlogin"));
    let (replication, bypassrls) = (r.bool("rolreplication"), r.bool("rolbypassrls"));
    (0..r.catalog.len)
        .map(|i| RoleRow {
            oid: oid[i],
            name: name[i],
            attrs: crate::roles::Attrs {
                superuser: superuser[i],
                inherit: inherit[i],
                createrole: createrole[i],
                createdb: createdb[i],
                login: login[i],
                replication: replication[i],
                bypassrls: bypassrls[i],
            },
        })
        .collect()
}

/// The rows of `pg_auth_members`.
fn load_members() -> Vec<MemberRow> {
    let r = Rows::new("pg_auth_members");
    if r.catalog.len == 0 {
        return Vec::new();
    }
    let (role, member) = (r.oid("roleid"), r.oid("member"));
    let (admin, inherit, set) =
        (r.bool("admin_option"), r.bool("inherit_option"), r.bool("set_option"));
    (0..r.catalog.len)
        .map(|i| MemberRow {
            role: role[i],
            member: member[i],
            admin: admin[i],
            inherit: inherit[i],
            set: set[i],
        })
        .collect()
}

/// The range type of each multirange type, from `pg_range`.
fn load_multirange_ranges() -> HashMap<u32, u32> {
    let r = Rows::new("pg_range");
    let (range, multirange) = (r.oid("rngtypid"), r.oid("rngmultitypid"));
    (0..r.catalog.len).map(|i| (multirange[i], range[i])).collect()
}

/// The subtype and the multirange type of each range type, from `pg_range`.
fn load_range_types() -> HashMap<u32, (u32, u32)> {
    let r = Rows::new("pg_range");
    let (range, subtype, multirange) =
        (r.oid("rngtypid"), r.oid("rngsubtype"), r.oid("rngmultitypid"));
    (0..r.catalog.len).map(|i| (range[i], (subtype[i], multirange[i]))).collect()
}

/// The values of an `aclitem[]` column, for [`Rows::optional`].
fn text_array(values: &'static Values) -> Option<&'static [&'static [&'static str]]> {
    match values {
        Values::TextArray(v) => Some(*v),
        _ => None,
    }
}

/// The rows of `pg_description` or `pg_shdescription`, in the order of the catalog.
fn load_descriptions(catalog: &str) -> Vec<DescriptionRow> {
    let r = Rows::new(catalog);
    let (objoid, classoid, description) =
        (r.oid("objoid"), r.oid("classoid"), r.text("description"));
    let objsubid = (catalog == "pg_description").then(|| r.int4("objsubid"));
    (0..r.catalog.len)
        .map(|i| DescriptionRow {
            objoid: objoid[i],
            classoid: classoid[i],
            objsubid: objsubid.map_or(0, |s| s[i]),
            description: description[i],
        })
        .collect()
}

/// The columns of the primary key of each table that has one that the database checks at once, from `pg_constraint`.
fn load_primary_keys() -> HashMap<u32, Vec<i16>> {
    let r = Rows::new("pg_constraint");
    let (relid, kind, deferrable) = (r.oid("conrelid"), r.char("contype"), r.bool("condeferrable"));
    let mut keys = HashMap::new();
    for i in 0..r.catalog.len {
        if kind[i] != b'p' || deferrable[i] {
            continue;
        }
        let columns = r.optional("conkey", i, |v| match v {
            Values::TextArray(v) => Some(*v),
            _ => None,
        });
        let columns = columns.unwrap_or_default().iter().map(|c| c.parse().expect("an attnum"));
        keys.insert(relid[i], columns.collect());
    }
    keys
}

/// Every built-in type, in the order of `pg_type.dat`.
pub fn types() -> &'static [TypeRow] {
    &BUILTIN.types
}

/// The built-in type with this OID, a type that [`add_system_type`] added, or a type that [`use_session_types`] gave to the thread.
pub fn type_by_oid(oid: u32) -> Option<&'static TypeRow> {
    BUILTIN
        .type_oid
        .get(&oid)
        .map(|&i| &BUILTIN.types[i])
        .or_else(|| system().types.iter().copied().find(|t| t.oid == oid))
        .or_else(|| SESSION.with_borrow(|s| s.types.iter().copied().find(|t| t.oid == oid)))
}

/// The built-in type with this name in this schema, a type that [`add_system_type`] added, or a type that [`use_session_types`] gave to the thread.
pub fn type_by_name(namespace: u32, name: &str) -> Option<&'static TypeRow> {
    let b = &*BUILTIN;
    let builtin = b
        .type_name
        .get(name)
        .and_then(|rows| rows.iter().map(|&i| &b.types[i]).find(|t| t.namespace == namespace));
    let named = |t: &&TypeRow| t.namespace == namespace && t.name == name;
    builtin
        .or_else(|| system().types.iter().copied().find(named))
        .or_else(|| SESSION.with_borrow(|s| s.types.iter().copied().find(named)))
}

/// The constraints of a domain that `initdb` makes with SQL, or of a domain of the catalog of the session.
#[derive(Debug)]
pub struct DomainRow {
    /// The OID of the domain.
    pub oid: u32,
    /// `typnotnull`: the domain does not accept null values.
    pub not_null: bool,
    /// The name and the expression of each check constraint, in the order of their OIDs. The expression is `conbin` in the form of the analyzer.
    pub checks: Vec<(String, String)>,
}

/// The types, the domains and the functions that `initdb` makes with SQL after the bootstrap, such as the domains and the functions of `information_schema`.
struct System {
    types: Vec<&'static TypeRow>,
    domains: Vec<&'static DomainRow>,
    procs: Vec<&'static ProcRow>,
}

/// The setup of a new cluster fills this once in the process, before the first session. Each database has the same objects of `initdb`, so the rows are the same for all of them, as the built-in rows are.
static SYSTEM: RwLock<System> =
    RwLock::new(System { types: Vec::new(), domains: Vec::new(), procs: Vec::new() });

fn system() -> std::sync::RwLockReadGuard<'static, System> {
    SYSTEM.read().unwrap_or_else(PoisonError::into_inner)
}

/// Adds a type that `initdb` makes with SQL, so that [`type_by_oid`] and [`type_by_name`] find it. A type with the OID of a known type changes nothing.
pub fn add_system_type(row: TypeRow) {
    if type_by_oid(row.oid).is_some() {
        return;
    }
    // The rows live as long as the process, as the built-in rows do.
    SYSTEM.write().unwrap_or_else(PoisonError::into_inner).types.push(Box::leak(Box::new(row)));
}

/// Adds the constraints of a domain that `initdb` makes with SQL. A domain that has its constraints already changes nothing.
pub fn add_system_domain(domain: DomainRow) {
    if domain_by_oid(domain.oid).is_some() {
        return;
    }
    SYSTEM
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .domains
        .push(Box::leak(Box::new(domain)));
}

/// Adds a function that `initdb` makes with SQL, so that [`proc_by_oid`] and [`procs_named`] find it. A function with the OID of a known function changes nothing.
pub fn add_system_proc(row: ProcRow) {
    if proc_by_oid(row.oid).is_some() {
        return;
    }
    SYSTEM.write().unwrap_or_else(PoisonError::into_inner).procs.push(Box::leak(Box::new(row)));
}

/// True for a function that [`add_system_proc`] added. Such a function is in SQL, and the engine runs its body.
pub fn is_system_proc(oid: u32) -> bool {
    !BUILTIN.proc_oid.contains_key(&oid) && system().procs.iter().any(|p| p.oid == oid)
}

/// The constraints of a domain that [`add_system_domain`] added, or of a domain that [`use_session_types`] gave to the thread.
pub fn domain_by_oid(oid: u32) -> Option<&'static DomainRow> {
    system()
        .domains
        .iter()
        .copied()
        .find(|d| d.oid == oid)
        .or_else(|| SESSION.with_borrow(|s| s.domains.iter().copied().find(|d| d.oid == oid)))
}

/// A domain of the catalog of a session, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDomain {
    /// The facts of the rows of the domain and of its array type.
    pub rows: NewDomainRows,
    /// `typnotnull`.
    pub not_null: bool,
    /// The name and the expression of each check constraint, as in [`DomainRow`].
    pub checks: Vec<(String, String)>,
}

/// The row type of a relation of the catalog of a session, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRowType {
    pub oid: u32,
    pub name: String,
    pub array: u32,
    pub array_name: String,
    pub namespace: u32,
    /// `typrelid`: the relation of the row type.
    pub relation: u32,
}

/// An enum type of the catalog of a session, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEnum {
    pub oid: u32,
    pub name: String,
    pub array: u32,
    pub array_name: String,
    pub namespace: u32,
    /// The OID and the name of each label, in their sort order.
    pub labels: Vec<(u32, String)>,
    /// The OIDs of the labels that the open transaction added, which a statement cannot use before the commit.
    pub uncommitted: Vec<u32>,
}

/// The labels of an enum type that [`use_session_types`] gave to the thread, for [`enum_labels`].
#[derive(Debug)]
struct EnumRow {
    oid: u32,
    labels: Vec<(u32, String)>,
    uncommitted: Vec<u32>,
}

/// A range type of the catalog of a session, with its multirange type, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRange {
    pub oid: u32,
    pub name: String,
    pub array: u32,
    pub array_name: String,
    pub namespace: u32,
    /// `rngsubtype`.
    pub subtype: u32,
    /// `rngcollation`.
    pub collation: u32,
    /// `typalign` of the four types.
    pub align: u8,
    pub multirange: u32,
    pub multirange_name: String,
    pub multirange_namespace: u32,
    pub multirange_array: u32,
    pub multirange_array_name: String,
    /// The OIDs of the two constructors of the range type, then of the three constructors of the multirange type.
    pub constructors: [u32; 5],
    /// The OID of the cast from the range type to the multirange type.
    pub cast: u32,
}

/// The facts of `pg_range` for a range type that [`use_session_types`] gave to the thread.
#[derive(Debug)]
struct RangeRow {
    oid: u32,
    subtype: u32,
    multirange: u32,
    collation: u32,
}

/// A function in SQL of the catalog of a session, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFunction {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub lang: u32,
    pub strict: bool,
    pub retset: bool,
    pub volatile: u8,
    /// `prokind`: `f` for a function or `p` for a procedure.
    pub kind: u8,
    pub rettype: u32,
    pub argtypes: Vec<u32>,
    /// `proallargtypes`, or empty if all arguments are inputs.
    pub allargtypes: Vec<u32>,
    /// `proargmodes`, or empty if all arguments are inputs.
    pub argmodes: Vec<u8>,
    /// `proargnames`, or empty if no argument has a name.
    pub argnames: Vec<String>,
    /// `provariadic`: the element type of the `VARIADIC` argument, or 0.
    pub variadic: u32,
    /// `proargdefaults` in the stored form of `rupg-analyze`, or `None` if no argument has a default.
    pub argdefaults: Option<String>,
    pub nargdefaults: i16,
    /// `prosrc` of a function in C or in `internal`, the symbol that runs it, or empty for another language.
    pub src: String,
}

/// An operator of the catalog of a session, for [`use_session_types`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOperator {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    /// `oprkind`: `b` for an infix operator or `l` for a prefix operator.
    pub kind: u8,
    pub can_merge: bool,
    pub can_hash: bool,
    pub left: u32,
    pub right: u32,
    pub result: u32,
    pub commutator: u32,
    pub negator: u32,
    /// `oprcode`, or 0 for a shell.
    pub code: u32,
}

/// An object of the catalog of a session that has rows in the overlay of the thread: a type, a function, or an operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionType {
    Domain(SessionDomain),
    Row(SessionRowType),
    Enum(SessionEnum),
    Range(SessionRange),
    Function(SessionFunction),
    Operator(SessionOperator),
}

/// The types of the catalog that the session of the thread uses now.
#[derive(Default)]
struct Session {
    /// The types as the session gave them.
    given: Vec<SessionType>,
    types: Vec<&'static TypeRow>,
    domains: Vec<&'static DomainRow>,
    enums: Vec<&'static EnumRow>,
    ranges: Vec<&'static RangeRow>,
    procs: Vec<&'static ProcRow>,
    casts: Vec<&'static CastRow>,
    operators: Vec<&'static OperatorRow>,
}

thread_local! {
    /// A session runs each statement on one thread, so the rows of its types are the rows of the thread.
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
}

/// The rows of a type of [`INTERNED`]: the type, its array type and the other types that come with it, the constraints of a domain, the labels of an enum type, the facts of a range type, and the functions and the casts that come with the type.
#[derive(Clone, Default)]
struct Interned {
    types: Vec<&'static TypeRow>,
    domain: Option<&'static DomainRow>,
    labels: Option<&'static EnumRow>,
    range: Option<&'static RangeRow>,
    procs: Vec<&'static ProcRow>,
    casts: Vec<&'static CastRow>,
    operator: Option<&'static OperatorRow>,
}

/// The rows of each type that a session used, with the facts that made them. A type gets its rows once in the process, also when many sessions use it, so the rows live as long as the process, as the built-in rows do.
static INTERNED: Mutex<Vec<(SessionType, Interned)>> = Mutex::new(Vec::new());

/// The rows of a type of a session. `None` if a type that the rows copy is not known.
fn session_rows(ty: &SessionType) -> Option<Interned> {
    let leak = |row: TypeRow| -> &'static TypeRow { Box::leak(Box::new(row)) };
    Some(match ty {
        SessionType::Domain(domain) => {
            let (row, array) = domain_rows(domain.rows.clone())?;
            let constraints = DomainRow {
                oid: row.oid,
                not_null: domain.not_null,
                checks: domain.checks.clone(),
            };
            Interned {
                types: vec![leak(row), leak(array)],
                domain: Some(Box::leak(Box::new(constraints))),
                ..Interned::default()
            }
        }
        SessionType::Row(new) => {
            let (row, array) = row_type_rows(new.clone())?;
            Interned { types: vec![leak(row), leak(array)], ..Interned::default() }
        }
        SessionType::Enum(new) => {
            let (row, array) = enum_rows(new.clone())?;
            let labels = EnumRow {
                oid: new.oid,
                labels: new.labels.clone(),
                uncommitted: new.uncommitted.clone(),
            };
            Interned {
                types: vec![leak(row), leak(array)],
                labels: Some(Box::leak(Box::new(labels))),
                ..Interned::default()
            }
        }
        SessionType::Range(new) => range_rows(new)?,
        SessionType::Function(new) => {
            Interned { procs: vec![Box::leak(Box::new(function_row(new)))], ..Interned::default() }
        }
        SessionType::Operator(new) => {
            let row = OperatorRow {
                oid: new.oid,
                name: new.name.clone().leak(),
                namespace: new.namespace,
                kind: new.kind,
                can_merge: new.can_merge,
                can_hash: new.can_hash,
                left: new.left,
                right: new.right,
                result: new.result,
                commutator: new.commutator,
                negator: new.negator,
                code: new.code,
            };
            Interned { operator: Some(Box::leak(Box::new(row))), ..Interned::default() }
        }
    })
}

/// The row of `pg_proc` of a function of a session. The analyzer reads the body of a function in SQL from the catalog of the session, so the row has no `prosqlbody`, and `prosrc` only for a function in C or in `internal`.
fn function_row(new: &SessionFunction) -> ProcRow {
    ProcRow {
        oid: new.oid,
        name: new.name.clone().leak(),
        namespace: new.namespace,
        lang: new.lang,
        variadic: new.variadic,
        kind: new.kind,
        strict: new.strict,
        retset: new.retset,
        volatile: new.volatile,
        nargs: i16::try_from(new.argtypes.len()).unwrap_or(i16::MAX),
        nargdefaults: new.nargdefaults,
        rettype: new.rettype,
        argtypes: new.argtypes.clone().leak(),
        allargtypes: (!new.allargtypes.is_empty()).then(|| &*new.allargtypes.clone().leak()),
        argmodes: (!new.argmodes.is_empty()).then(|| &*new.argmodes.clone().leak()),
        argnames: (!new.argnames.is_empty()).then(|| {
            let names: Vec<&'static str> =
                new.argnames.iter().map(|n| &*n.clone().leak()).collect();
            &*names.leak()
        }),
        argdefaults: new.argdefaults.clone().map(|text| &*text.leak()),
        src: new.src.clone().leak(),
        sqlbody: None,
    }
}

/// Gives the types of the catalog of the session to the thread, so that [`type_by_oid`], [`type_by_name`] and [`domain_by_oid`] find them. The types come in the order of their OIDs, so the base type of a domain comes before the domain. A type that copies a type that is not known has no rows.
pub fn use_session_types(types: Vec<SessionType>) {
    if SESSION.with_borrow(|s| s.given == types) {
        return;
    }
    SESSION.set(Session::default());
    for ty in types {
        let interned = INTERNED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(facts, _)| *facts == ty)
            .map(|(_, rows)| rows.clone());
        // `domain_rows` reads the base type from the rows of the thread, so it runs with no borrow of them.
        let rows = interned.or_else(|| {
            let rows = session_rows(&ty)?;
            INTERNED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((ty.clone(), rows.clone()));
            Some(rows)
        });
        SESSION.with_borrow_mut(|s| {
            if let Some(rows) = rows {
                s.types.extend(rows.types);
                s.domains.extend(rows.domain);
                s.enums.extend(rows.labels);
                s.ranges.extend(rows.range);
                s.procs.extend(rows.procs);
                s.casts.extend(rows.casts);
                s.operators.extend(rows.operator);
            }
            s.given.push(ty);
        });
    }
}

/// The rows of the row type of a relation and of its array type, which copy the rows of the row type of `pg_class`, as all row types have the same form.
fn row_type_rows(new: SessionRowType) -> Option<(TypeRow, TypeRow)> {
    let template = type_by_oid(catalog("pg_class")?.rowtype_oid)?;
    let row = TypeRow {
        oid: new.oid,
        name: new.name.leak(),
        namespace: new.namespace,
        relid: new.relation,
        array: new.array,
        ..*template
    };
    let array = TypeRow {
        oid: new.array,
        name: new.array_name.leak(),
        namespace: new.namespace,
        elem: new.oid,
        ..*type_by_oid(template.array)?
    };
    Some((row, array))
}

/// The OID of `oid`, the template of an enum type, which has the length, the alignment and the storage of `oid`.
const OID_TYPE: u32 = 26;

/// The rows of an enum type and of its array type, as `DefineEnum` makes them.
fn enum_rows(new: SessionEnum) -> Option<(TypeRow, TypeRow)> {
    let template = type_by_oid(OID_TYPE)?;
    let function = |name: &str| procs_named(name).next().map(|p| p.oid);
    let row = TypeRow {
        oid: new.oid,
        name: new.name.leak(),
        namespace: new.namespace,
        kind: b'e',
        category: b'E',
        preferred: false,
        array: new.array,
        input: function("enum_in")?,
        output: function("enum_out")?,
        receive: function("enum_recv")?,
        send: function("enum_send")?,
        ..*template
    };
    let array = TypeRow {
        oid: new.array,
        name: new.array_name.leak(),
        namespace: new.namespace,
        elem: new.oid,
        ..*type_by_oid(template.array)?
    };
    Some((row, array))
}

/// The OID of the type `text`.
const TEXT: u32 = 25;

/// The OIDs of `int4range`, `_int4range`, `int4multirange` and `_int4multirange`, the templates of the rows of a range type, as `DefineRange` gives each range type the same functions.
const INT4_RANGE_TYPES: [u32; 4] = [3904, 3905, 4451, 6150];

/// The rows of a range type, of its multirange type, of their array types, of the constructors and of the cast, as `DefineRange` makes them. The rows copy the rows of `int4range`, with the alignment of the subtype. The constructors copy the constructors of `int4range` and `int4multirange`.
fn range_rows(new: &SessionRange) -> Option<Interned> {
    let [range, range_array, multirange, multirange_array] = INT4_RANGE_TYPES;
    let copy = |template: u32, oid: u32, name: &str, namespace: u32| -> Option<TypeRow> {
        Some(TypeRow {
            oid,
            name: name.to_string().leak(),
            namespace,
            align: new.align,
            ..*type_by_oid(template)?
        })
    };
    let types = vec![
        TypeRow { array: new.array, ..copy(range, new.oid, &new.name, new.namespace)? },
        TypeRow { elem: new.oid, ..copy(range_array, new.array, &new.array_name, new.namespace)? },
        TypeRow {
            array: new.multirange_array,
            ..copy(multirange, new.multirange, &new.multirange_name, new.multirange_namespace)?
        },
        TypeRow {
            elem: new.multirange,
            ..copy(
                multirange_array,
                new.multirange_array,
                &new.multirange_array_name,
                new.multirange_namespace,
            )?
        },
    ];
    let int4 = type_by_oid(range)?.name;
    let int4_multirange = type_by_oid(multirange)?.name;
    let leak_args = |args: Vec<u32>| -> &'static [u32] { args.leak() };
    let sub = new.subtype;
    let shapes = [
        (int4, 2, new.oid, vec![sub, sub]),
        (int4, 3, new.oid, vec![sub, sub, TEXT]),
        (int4_multirange, 0, new.multirange, Vec::new()),
        (int4_multirange, 1, new.multirange, vec![new.oid]),
        (int4_multirange, 1, new.multirange, vec![new.array]),
    ];
    let mut procs = Vec::with_capacity(shapes.len());
    for (i, (name, nargs, rettype, args)) in shapes.into_iter().enumerate() {
        // The two constructors of a multirange type with one argument differ by `VARIADIC`.
        let base = procs_named(name)
            .find(|p| p.argtypes.len() == nargs && (i < 3 || (p.variadic != 0) == (i == 4)))?;
        let own_name = if i < 2 { &new.name } else { &new.multirange_name };
        let row = ProcRow {
            oid: new.constructors[i],
            name: own_name.clone().leak(),
            namespace: new.namespace,
            rettype,
            variadic: if base.variadic == 0 { 0 } else { new.oid },
            argtypes: leak_args(args.clone()),
            allargtypes: base.allargtypes.map(|_| leak_args(args)),
            ..*base
        };
        procs.push(&*Box::leak(Box::new(row)));
    }
    let cast = CastRow {
        oid: new.cast,
        source: new.oid,
        target: new.multirange,
        func: new.constructors[3],
        context: b'e',
        method: b'f',
    };
    let facts = RangeRow {
        oid: new.oid,
        subtype: new.subtype,
        multirange: new.multirange,
        collation: new.collation,
    };
    Some(Interned {
        types: types.into_iter().map(|row| &*Box::leak(Box::new(row))).collect(),
        range: Some(Box::leak(Box::new(facts))),
        procs,
        casts: vec![Box::leak(Box::new(cast))],
        ..Interned::default()
    })
}

/// The OID and the name of each label of an enum type that [`use_session_types`] gave to the thread, in their sort order. `None` for a type that is not such an enum type.
pub fn enum_labels(ty: u32) -> Option<&'static [(u32, String)]> {
    SESSION.with_borrow(|s| s.enums.iter().find(|e| e.oid == ty).map(|e| e.labels.as_slice()))
}

/// The place of a label in the sort order of its enum type, as `enum_cmp_internal` reads it from the type cache for an odd OID. `None` for an OID that is not a label of an enum type that [`use_session_types`] gave to the thread.
pub fn enum_label_place(label: u32) -> Option<usize> {
    SESSION.with_borrow(|s| s.enums.iter().find_map(|e| e.labels.iter().position(|l| l.0 == label)))
}

/// `EnumUncommitted`: true for a label that the open transaction added to an enum type that an earlier transaction made.
pub fn enum_label_is_uncommitted(ty: u32, label: u32) -> bool {
    SESSION.with_borrow(|s| s.enums.iter().any(|e| e.oid == ty && e.uncommitted.contains(&label)))
}

/// The OID of `_int4`, the template of the array type of a domain over a type with no array type.
const INT4_ARRAY: u32 = 1007;

/// The OID of `domain_in`, the input function of every domain.
pub const DOMAIN_IN: u32 = 2597;
/// The OID of `domain_recv`, the receive function of every domain.
pub const DOMAIN_RECV: u32 = 2598;

/// The facts of a new domain for [`domain_rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewDomainRows {
    pub oid: u32,
    pub name: String,
    pub array: u32,
    pub array_name: String,
    pub namespace: u32,
    pub base: u32,
    pub typmod: i32,
    pub collation: u32,
}

/// The rows of a domain and of its array type, as `DefineDomain` makes them from the row of the base type and the row of its array type. `None` if the base type or its array type is not known. The names live as long as the process, as the names of the built-in rows do.
pub fn domain_rows(new: NewDomainRows) -> Option<(TypeRow, TypeRow)> {
    let base = *type_by_oid(new.base)?;
    // A base type with no array type, such as an array type, gives the array type of the domain the alignment of `DefineDomain`.
    let mut array = match type_by_oid(base.array) {
        Some(array) => *array,
        None => TypeRow {
            align: if base.align == b'd' { b'd' } else { b'i' },
            ..*type_by_oid(INT4_ARRAY)?
        },
    };
    let domain = TypeRow {
        oid: new.oid,
        name: new.name.leak(),
        namespace: new.namespace,
        kind: b'd',
        preferred: false,
        relid: 0,
        subscript: 0,
        elem: 0,
        array: new.array,
        input: DOMAIN_IN,
        receive: DOMAIN_RECV,
        modin: 0,
        modout: 0,
        base: new.base,
        typmod: new.typmod,
        collation: new.collation,
        ..base
    };
    array.oid = new.array;
    array.name = new.array_name.leak();
    array.namespace = new.namespace;
    array.elem = new.oid;
    array.collation = new.collation;
    // `DefineDomain` gives the array type no typmod functions.
    array.modin = 0;
    array.modout = 0;
    Some((domain, array))
}

/// Every built-in function, in the order of `pg_proc.dat`.
pub fn procs() -> &'static [ProcRow] {
    &BUILTIN.procs
}

/// The built-in function with this OID, a function that [`add_system_proc`] added, or a function of a type that [`use_session_types`] gave to the thread.
pub fn proc_by_oid(oid: u32) -> Option<&'static ProcRow> {
    BUILTIN
        .proc_oid
        .get(&oid)
        .map(|&i| &BUILTIN.procs[i])
        .or_else(|| system().procs.iter().copied().find(|p| p.oid == oid))
        .or_else(|| SESSION.with_borrow(|s| s.procs.iter().copied().find(|p| p.oid == oid)))
}

/// The built-in functions with this name, in the order of `pg_proc.dat`, then the functions with this name that [`add_system_proc`] added, then the functions with this name of the types that [`use_session_types`] gave to the thread.
pub fn procs_named(name: &str) -> impl Iterator<Item = &'static ProcRow> {
    let b = &*BUILTIN;
    let mut added: Vec<&'static ProcRow> =
        system().procs.iter().copied().filter(|p| p.name == name).collect();
    SESSION.with_borrow(|s| added.extend(s.procs.iter().copied().filter(|p| p.name == name)));
    b.proc_name.get(name).into_iter().flatten().map(|&i| &b.procs[i]).chain(added)
}

/// Every built-in operator.
pub fn operators() -> &'static [OperatorRow] {
    &BUILTIN.operators
}

/// The built-in operator with this OID, or an operator that [`use_session_types`] gave to the thread.
pub fn operator_by_oid(oid: u32) -> Option<&'static OperatorRow> {
    BUILTIN
        .operator_oid
        .get(&oid)
        .map(|&i| &BUILTIN.operators[i])
        .or_else(|| SESSION.with_borrow(|s| s.operators.iter().copied().find(|o| o.oid == oid)))
}

/// The built-in operators with this name, then the operators with this name that [`use_session_types`] gave to the thread.
pub fn operators_named(name: &str) -> impl Iterator<Item = &'static OperatorRow> {
    let b = &*BUILTIN;
    let added: Vec<&'static OperatorRow> =
        SESSION.with_borrow(|s| s.operators.iter().copied().filter(|o| o.name == name).collect());
    b.operator_name.get(name).into_iter().flatten().map(|&i| &b.operators[i]).chain(added)
}

/// Every built-in cast.
pub fn casts() -> &'static [CastRow] {
    &BUILTIN.casts
}

/// The built-in cast from `source` to `target`, or a cast of a type that [`use_session_types`] gave to the thread.
pub fn cast(source: u32, target: u32) -> Option<&'static CastRow> {
    BUILTIN.cast_pair.get(&(source, target)).map(|&i| &BUILTIN.casts[i]).or_else(|| {
        SESSION.with_borrow(|s| {
            s.casts.iter().copied().find(|c| c.source == source && c.target == target)
        })
    })
}

/// Every built-in operator class.
pub fn opclasses() -> &'static [OpclassRow] {
    &BUILTIN.opclasses
}

/// `get_opfamily_member`: the search operator of the family for the two types and the strategy.
pub fn opfamily_member(family: u32, left: u32, right: u32, strategy: i16) -> Option<u32> {
    let b = &*BUILTIN;
    b.amop_member.get(&(family, left, right, strategy)).map(|&i| b.amops[i].operator)
}

/// The rows of `pg_amop` for this operator.
pub fn amops_of_operator(operator: u32) -> impl Iterator<Item = &'static AmopRow> {
    let b = &*BUILTIN;
    b.amop_operator.get(&operator).into_iter().flatten().map(|&i| &b.amops[i])
}

/// The rows of `pg_amproc` of the family for the left type, as `SearchSysCacheList2(AMPROCNUM, ...)` finds them.
pub fn amprocs(family: u32, left: u32) -> &'static [AmprocRow] {
    BUILTIN.amprocs.get(&(family, left)).map_or(&[], Vec::as_slice)
}

/// The row of `pg_amop` with this OID.
pub fn amop_by_oid(oid: u32) -> Option<&'static AmopRow> {
    BUILTIN.amops.iter().find(|a| a.oid == oid)
}

/// The row of `pg_amproc` with this OID.
pub fn amproc_by_oid(oid: u32) -> Option<&'static AmprocRow> {
    BUILTIN.amprocs.values().flatten().find(|a| a.oid == oid)
}

/// The name of the access method with this OID, from `pg_am`.
pub fn access_method_name(oid: u32) -> Option<&'static str> {
    let r = Rows::new("pg_am");
    let at = r.oid("oid").iter().position(|&o| o == oid)?;
    Some(r.text("amname")[at])
}

/// The row of `pg_aggregate` of the aggregate function with this OID.
pub fn aggregate(fnoid: u32) -> Option<&'static AggregateRow> {
    BUILTIN.aggregate_fn.get(&fnoid).map(|&i| &BUILTIN.aggregates[i])
}

/// The built-in objects of a kind, in the order of the `.dat` file.
pub fn named(kind: Named) -> &'static [NamedRow] {
    BUILTIN.named.get(&kind).map_or(&[], Vec::as_slice)
}

/// The built-in object of a kind with this OID.
pub fn named_by_oid(kind: Named, oid: u32) -> Option<&'static NamedRow> {
    named(kind).iter().find(|row| row.oid == oid)
}

/// The comment of an object from `pg_description`: the OID of the object, the OID of its catalog and the column number, or 0 for the object itself.
pub fn description(objoid: u32, classoid: u32, objsubid: i32) -> Option<&'static str> {
    BUILTIN
        .description_key
        .get(&(objoid, classoid, objsubid))
        .map(|&i| BUILTIN.descriptions[i].description)
}

/// The rows of `pg_description`, in the order of the catalog.
pub fn descriptions() -> &'static [DescriptionRow] {
    &BUILTIN.descriptions
}

/// The comment of a shared object from `pg_shdescription`: the OID of the object and the OID of its catalog.
pub fn shared_description(objoid: u32, classoid: u32) -> Option<&'static str> {
    BUILTIN
        .shared_descriptions
        .iter()
        .find(|d| d.objoid == objoid && d.classoid == classoid)
        .map(|d| d.description)
}

/// The attribute numbers of the primary key of the table, when the key is not deferrable, as `check_functional_grouping` finds them.
pub fn primary_key(relid: u32) -> Option<&'static [i16]> {
    BUILTIN.primary_keys.get(&relid).map(Vec::as_slice)
}

/// The built-in relations, in the order of the catalog.
pub fn classes() -> &'static [ClassRow] {
    &BUILTIN.classes
}

/// The relation with this OID.
pub fn class_by_oid(oid: u32) -> Option<&'static ClassRow> {
    BUILTIN.class_oid.get(&oid).map(|&i| &BUILTIN.classes[i])
}

/// The columns of a relation, with the system columns, in the order of the catalog.
pub fn attributes(relid: u32) -> &'static [AttributeRow] {
    BUILTIN.attributes.get(&relid).map_or(&[], Vec::as_slice)
}

/// The objects of a kind that has an owner.
pub fn owned(kind: Owned) -> &'static [OwnedRow] {
    BUILTIN.owned.get(&kind).map_or(&[], Vec::as_slice)
}

/// The object of a kind that has an owner with this OID.
pub fn owned_by_oid(kind: Owned, oid: u32) -> Option<&'static OwnedRow> {
    BUILTIN.owned_oid.get(&(kind, oid)).map(|&i| &BUILTIN.owned[&kind][i])
}

/// The roles, in the order of `pg_authid`.
pub fn roles() -> &'static [RoleRow] {
    &BUILTIN.roles
}

/// The memberships of roles, in the order of `pg_auth_members`.
pub fn members() -> &'static [MemberRow] {
    &BUILTIN.members
}

/// The facts of a range type that [`use_session_types`] gave to the thread that `found` takes.
fn session_range<T>(found: impl Fn(&RangeRow) -> Option<T>) -> Option<T> {
    SESSION.with_borrow(|s| s.ranges.iter().find_map(|r| found(r)))
}

/// The range type of a multirange type, as `get_multirange_range` gives it.
pub fn multirange_range(multirange: u32) -> Option<u32> {
    BUILTIN
        .multirange_range
        .get(&multirange)
        .copied()
        .or_else(|| session_range(|r| (r.multirange == multirange).then_some(r.oid)))
}

/// The subtype of a range type, as `get_range_subtype` gives it.
pub fn range_subtype(range: u32) -> Option<u32> {
    BUILTIN
        .range_types
        .get(&range)
        .map(|&(subtype, _)| subtype)
        .or_else(|| session_range(|r| (r.oid == range).then_some(r.subtype)))
}

/// The multirange type of a range type, as `get_range_multirange` gives it.
pub fn range_multirange(range: u32) -> Option<u32> {
    BUILTIN
        .range_types
        .get(&range)
        .map(|&(_, multirange)| multirange)
        .or_else(|| session_range(|r| (r.oid == range).then_some(r.multirange)))
}

/// The collation of a range type of a session, as `get_range_collation` gives it, or `None` for a built-in range type, whose collation is the collation of its subtype.
pub fn range_collation(range: u32) -> Option<u32> {
    session_range(|r| (r.oid == range).then_some(r.collation))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows() {
        let int4 = type_by_oid(23).unwrap();
        assert_eq!((int4.name, int4.len, int4.category, int4.array), ("int4", 4, b'N', 1007));
        assert!(type_by_oid(701).unwrap().preferred);
        assert_eq!(type_by_name(11, "text").unwrap().oid, 25);
        assert_eq!(type_by_oid(1007).unwrap().elem, 23);
        let plus = operators_named("+").find(|o| o.left == 23 && o.right == 23).unwrap();
        assert_eq!((plus.result, proc_by_oid(plus.code).unwrap().src), (23, "int4pl"));
        let lower = procs_named("lower").find(|p| p.argtypes == [25]).unwrap();
        assert_eq!((lower.rettype, lower.volatile, lower.strict), (25, b'i', true));
        let to_int8 = cast(23, 20).unwrap();
        assert_eq!((to_int8.context, to_int8.method), (b'i', b'f'));
        assert_eq!(cast(25, 1043).unwrap().method, b'b');
        let int4_ops = opclasses().iter().find(|c| c.method == 403 && c.intype == 23 && c.default);
        let family = int4_ops.unwrap().family;
        assert_eq!(opfamily_member(family, 23, 23, 1), Some(97));
        assert_eq!(opfamily_member(family, 23, 23, 5), Some(521));
        assert!(amops_of_operator(97).any(|a| a.method == 403 && a.strategy == 1));
        // in_range_int4_int8 of integer_ops.
        assert!(
            amprocs(family, 23).iter().any(|a| a.number == 3 && a.right == 20 && a.proc == 4127)
        );
        let set_config = procs_named("set_config").next().unwrap();
        assert_eq!(set_config.volatile, b'v');
        let keywords = procs_named("pg_get_keywords").next().unwrap().argnames.unwrap().to_vec();
        assert_eq!(keywords, ["word", "catcode", "barelabel", "catdesc", "baredesc"]);
        let count = procs_named("count").find(|p| p.argtypes.is_empty()).unwrap();
        let row = aggregate(count.oid).unwrap();
        assert_eq!(
            (row.kind, proc_by_oid(row.transfn).unwrap().src, row.initval),
            (b'n', "int8inc", Some("0"))
        );
        let max = procs_named("max").find(|p| p.argtypes == [23]).unwrap();
        assert_eq!(aggregate(max.oid).unwrap().initval, None);
        // pg_class has the primary key (oid).
        assert_eq!(primary_key(1259), Some(&[1][..]));
    }
}

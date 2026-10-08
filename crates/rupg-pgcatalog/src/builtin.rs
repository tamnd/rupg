//! Typed rows of the built-in types, functions, operators, casts and operator classes, with indexes by OID and by name.
//!
//! The analyzer resolves names, operators, functions and casts with these rows, as `parse_oper.c`, `parse_func.c` and `parse_coerce.c` do with the syscache of PostgreSQL. It finds the sort and equality operators of a type with the rows of `pg_opclass` and `pg_amop`, as `typcache.c` does. The rows are read once from the static batches of the catalogs, so they hold the values of the pin and nothing else.

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::{Batch, Catalog, Values, catalog};

/// A row of `pg_type`.
#[derive(Debug)]
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
#[derive(Debug)]
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
    /// `proargdefaults` as the text of the node tree, or `None`.
    pub argdefaults: Option<&'static str>,
    /// `prosrc`: for an internal function, the name of the C function, which is the name of its kernel.
    pub src: &'static str,
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
    pub family: u32,
    pub left: u32,
    pub right: u32,
    pub strategy: i16,
    /// `amoppurpose`: `s` for search or `o` for ordering.
    pub purpose: u8,
    pub operator: u32,
    pub method: u32,
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

/// A row of `pg_class`, with the columns that the privilege checks read.
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
    /// `rolsuper`.
    pub superuser: bool,
}

/// A row of `pg_auth_members`: `member` is a member of `role`.
#[derive(Debug)]
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
    let (family, left, right) =
        (r.oid("amopfamily"), r.oid("amoplefttype"), r.oid("amoprighttype"));
    let (strategy, purpose) = (r.int2("amopstrategy"), r.char("amoppurpose"));
    let (operator, method) = (r.oid("amopopr"), r.oid("amopmethod"));
    (0..r.catalog.len)
        .map(|i| AmopRow {
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
    (0..r.catalog.len)
        .map(|i| ClassRow {
            oid: oid[i],
            name: name[i],
            namespace: namespace[i],
            kind: kind[i],
            natts: natts[i],
            owner: owner[i],
            acl: r.optional("relacl", i, text_array),
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
    (0..r.catalog.len)
        .map(|i| RoleRow { oid: oid[i], name: name[i], superuser: superuser[i] })
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

/// The built-in type with this OID.
pub fn type_by_oid(oid: u32) -> Option<&'static TypeRow> {
    BUILTIN.type_oid.get(&oid).map(|&i| &BUILTIN.types[i])
}

/// The built-in type with this name in this schema.
pub fn type_by_name(namespace: u32, name: &str) -> Option<&'static TypeRow> {
    let b = &*BUILTIN;
    b.type_name.get(name)?.iter().map(|&i| &b.types[i]).find(|t| t.namespace == namespace)
}

/// Every built-in function, in the order of `pg_proc.dat`.
pub fn procs() -> &'static [ProcRow] {
    &BUILTIN.procs
}

/// The built-in function with this OID.
pub fn proc_by_oid(oid: u32) -> Option<&'static ProcRow> {
    BUILTIN.proc_oid.get(&oid).map(|&i| &BUILTIN.procs[i])
}

/// The built-in functions with this name, in the order of `pg_proc.dat`.
pub fn procs_named(name: &str) -> impl Iterator<Item = &'static ProcRow> {
    let b = &*BUILTIN;
    b.proc_name.get(name).into_iter().flatten().map(|&i| &b.procs[i])
}

/// Every built-in operator.
pub fn operators() -> &'static [OperatorRow] {
    &BUILTIN.operators
}

/// The built-in operator with this OID.
pub fn operator_by_oid(oid: u32) -> Option<&'static OperatorRow> {
    BUILTIN.operator_oid.get(&oid).map(|&i| &BUILTIN.operators[i])
}

/// The built-in operators with this name.
pub fn operators_named(name: &str) -> impl Iterator<Item = &'static OperatorRow> {
    let b = &*BUILTIN;
    b.operator_name.get(name).into_iter().flatten().map(|&i| &b.operators[i])
}

/// Every built-in cast.
pub fn casts() -> &'static [CastRow] {
    &BUILTIN.casts
}

/// The built-in cast from `source` to `target`.
pub fn cast(source: u32, target: u32) -> Option<&'static CastRow> {
    BUILTIN.cast_pair.get(&(source, target)).map(|&i| &BUILTIN.casts[i])
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

/// The range type of a multirange type, as `get_multirange_range` gives it.
pub fn multirange_range(multirange: u32) -> Option<u32> {
    BUILTIN.multirange_range.get(&multirange).copied()
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

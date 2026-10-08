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

/// The attribute numbers of the primary key of the table, when the key is not deferrable, as `check_functional_grouping` finds them.
pub fn primary_key(relid: u32) -> Option<&'static [i16]> {
    BUILTIN.primary_keys.get(&relid).map(Vec::as_slice)
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

//! Typed rows of the built-in types, functions, operators and casts, with indexes by OID and by name.
//!
//! The analyzer resolves names, operators, functions and casts with these rows, as `parse_oper.c`, `parse_func.c` and `parse_coerce.c` do with the syscache of PostgreSQL. The rows are read once from the static batches of `pg_type`, `pg_proc`, `pg_operator` and `pg_cast`, so they hold the values of the pin and nothing else.

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

struct Builtin {
    types: Vec<TypeRow>,
    procs: Vec<ProcRow>,
    operators: Vec<OperatorRow>,
    casts: Vec<CastRow>,
    type_oid: HashMap<u32, usize>,
    type_name: HashMap<&'static str, Vec<usize>>,
    proc_oid: HashMap<u32, usize>,
    proc_name: HashMap<&'static str, Vec<usize>>,
    operator_oid: HashMap<u32, usize>,
    operator_name: HashMap<&'static str, Vec<usize>>,
    cast_pair: HashMap<(u32, u32), usize>,
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
            types,
            procs,
            operators,
            casts,
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
        let set_config = procs_named("set_config").next().unwrap();
        assert_eq!(set_config.volatile, b'v');
        let keywords = procs_named("pg_get_keywords").next().unwrap().argnames.unwrap().to_vec();
        assert_eq!(keywords, ["word", "catcode", "barelabel", "catdesc", "baredesc"]);
    }
}

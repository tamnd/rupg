// Lifted from rudb crates/rudb-qc-gen/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the module `codegen` of rupg-jit, as `gen` is a keyword in the 2024 edition, over the tree of `crate::plan` on PostgreSQL types, with a type's QIR type from `Type::data`. A function is translated by its `prosrc` with the answers and the errors of PostgreSQL: the integer arithmetic of `int4pl` and its kin raises `integer out of range` and the like, a division by zero raises `division by zero`, float arithmetic raises `value out of range: overflow` or `underflow` where `float8pl` and its kin do, and the casts are the functions `int48`, `i4toi2`, `dtoi4` and the rest. A function with no translator is refused, as rupg has no kernel of a first engine to call, so the module `vcall` is not lifted, and nor are decimals, unsigned integers, regular expressions, dates, `ILIKE`, `lower` and `upper`, which the runtime folds by Unicode where PostgreSQL in the C collation folds ASCII. A `LIKE` is translated only for a pattern with no backslash, as the runtime has no escape character. `AND` and `OR` skip their later children once one decides them when a later child calls a function, as PostgreSQL does. The aggregates take PostgreSQL's types: `sum` of an `int2` or `int4` is an `int8` and of an `int8` a `numeric`, `avg` of an integer is a `numeric`, `sum` of a `float8` checks for an overflow at each row, `min` and `max` of a float take the later of two equal values, and `sum` of a `float4`, `avg` of a float, a `min` or `max` of a type with no signed order and a `count(DISTINCT)` of a float are refused. A `bpchar` key is refused, as it compares without its trailing spaces. The ranges of the statistics only served decimals, so `Known::bits` and `Body::ranged` are not lifted. The tests build the trees by hand instead of parsing rudb plans, and check PostgreSQL's errors and the skipped children.

//! The generator, per `spec/compiler/07-code-generation.md`: one QIR function per pipeline.
//!
//! A body runs over the rows `begin..end` of one morsel. It reads the source columns through the morsel's column table, runs the operators in order and jumps to the next row at the first filter that is not true, and then hands the row to the sink. A join probe is a loop of its own inside the row loop, over the entries of the one directory slot the row's hash picks, and everything after it runs once per match, per section 10.5 of `spec/compiler/10-joins.md`. Everything the body keeps between calls is in its state, whose layout [`Body`] describes so that the driver can set it up and read it:
//!
//! ```text
//! [header: 64 bytes][sink fields]
//! ```
//!
//! A result sink has a row count and, per output column, the address of a values buffer and of a validity buffer with one byte per row. The driver points them at buffers as long as the morsel and turns what the body wrote into a chunk after each call. Past a probe a row can make any number of rows, so there the sink also has the buffers' capacity, and a body that fills them returns `NeedMemory` for the driver to grow them and run the morsel again. An aggregate sink has the key buffer the body builds each row's key in before `ht_insert`, or for an aggregate with no groups the address of the one group row, which the driver writes there before the first call. When every key is fixed width it also has the row of the last key and six words `ht_insert` publishes the table in, so that the body finds a key's group itself and calls only for a new one. A join build has the record buffer the body builds each row's record in before `jt_append`. After the sink come three words per probe, which the driver fills from the built table at init: the directory's address, the shift that takes a hash to its slot and the tag table's address.
//!
//! A `LIKE` of a source column against a constant is not asked a row at a time. The body reads it as one more column of the morsel, a byte a row, which the driver fills before the call by asking the pattern of the whole morsel at once, and [`Body::likes`] says which ones those are.
//!
//! Handles are made here, in the query's [`Rt`], because the code carries them as constants: the `LIKE` patterns, the grouping tables, the distinct sets. String literals are kept in the runtime's heap for the same reason. That is the literal table of document 07.
//!
//! A function is translated by its `prosrc`, with the answers and the errors of PostgreSQL's own function of that name, and a function with no translator here is refused by that name. An error is an error site of the module with PostgreSQL's message, which the driver raises with the SQLSTATE of its kind.
//!
//! Every value is a pair of a value and an `i1` that says whether it is valid, and the value under an invalid one is harmless: a column read puts a zero there, and every operation on harmless inputs gives a harmless output. A translator that can raise an error takes a zero under a null, or a one for a divisor, and checks only the rows whose arguments are valid, so that a null never raises one. It is also why this generator never records a validity for rule V12 to check.

use std::collections::BTreeMap;

use rupg_types::{Value, oid};

use crate::ir::catalogue::proxy;
use crate::ir::func::INV;
use crate::ir::status::NEED_MEMORY;
use crate::ir::{Block, Builder, ErrorKind, Field, Func, Module, Op, Ty, Val, dce, verify};
use crate::pipe::{Graph, Op as PipeOp, Pipeline, Probe, Sink, Stage};
use crate::plan::{Aggregate, CompareOp, Expr, JoinType, Kind, Refusal, Result, Runs, Type};
use crate::rt::abi::{
    COL_CODES, COL_SIZE, COL_VALID, HEADER, MORSEL_BEGIN, MORSEL_COLS, MORSEL_END,
};
use crate::rt::join::{ADDRESS, FOLD, JoinLayout, JoinTable};
use crate::rt::table::{GroupTable, KeyField, LANE_BITS, Layout, ROWS_PER_PAGE, SPREAD};
use crate::rt::{Ablate, Rt, text};

/// Where the sink's fields start.
const SINK: u32 = HEADER;

/// The module and what the driver needs to run each pipeline in it.
#[derive(Debug)]
pub struct Query {
    /// One function per pipeline.
    pub module: Module,
    /// One entry per stage of the graph, `None` for a stage that is not a pipeline.
    pub bodies: Vec<Option<Body>>,
}

/// One pipeline function and its state.
#[derive(Clone, Debug, PartialEq)]
pub struct Body {
    /// The function's name in the module.
    pub func: String,
    /// The source columns the body reads, in the order of the morsel's column table.
    pub reads: Vec<usize>,
    /// The bytes of state the body needs, header included.
    pub state: u32,
    /// Where the rows go.
    pub sink: Out,
    /// The join tables the body probes, in the order of its probes.
    pub probes: Vec<Probing>,
    /// The function for a morsel with no NULL in any column the body reads, which skips every
    /// validity check. It has the same state, sink and probes as [`Body::func`], so the driver
    /// can call either one on any morsel, and it is a pre-check guard of section 9.6 of
    /// `spec/compiler/09-tiering-and-caching.md` with `func` as its fallback.
    pub nonull: Option<String>,
    /// The `LIKE` answers the body reads after its source columns in the morsel's column table,
    /// in that order.
    pub likes: Vec<Matched>,
    /// The group key columns the body reads as the index of each row's value in the column's
    /// values, after the `LIKE` answers in the morsel's column table.
    pub domains: Vec<Domain>,
    /// The source columns that are not text but that the body reads through codes as it reads
    /// text, because the statistics said most of their rows are coded into a dictionary. The
    /// driver hands such a column as its dictionary's values and its codes when a morsel has it
    /// coded, and as its values and the codes `0, 1, 2, ...` when not.
    pub coded: Vec<usize>,
    /// For an aggregate by one key column read through codes, that column. The driver hands the
    /// body an array of group rows indexed by the key's code after [`Body::domains`] in the
    /// morsel's column table, kept by each worker for as long as the column comes with the same
    /// dictionary, or a null address for a morsel whose column is not coded. A row whose code has
    /// a row there takes it without hashing, and the way through the hash fills the entry in.
    pub keyed: Option<usize>,
}

/// A text column the driver hands the body as one `u16` a row: the index of the row's value in
/// [`Domain::values`], their count for a NULL, and one more than that for a value not in them.
#[derive(Clone, Debug, PartialEq)]
pub struct Domain {
    /// The source column.
    pub column: usize,
    /// Its values, as the statistics had them when the code was made.
    pub values: Vec<Vec<u8>>,
}

/// A `LIKE` of a source column against a constant, which the driver answers for the whole morsel
/// and hands the body as a column of one byte a row.
#[derive(Clone, Debug, PartialEq)]
pub struct Matched {
    /// The source column.
    pub column: usize,
    /// The pattern as written.
    pub pattern: String,
    /// For a comparison of the column against a constant rather than a `LIKE`, the orders of the
    /// column's text against the constant, less, equal and greater, that make it true. The
    /// constant is then [`Matched::pattern`].
    pub order: Option<[bool; 3]>,
    /// The pattern's handle in the runtime.
    pub like: u64,
}

/// Where the body reads a join table it probes, which the driver fills in before the first call.
#[derive(Clone, Debug, PartialEq)]
pub struct Probing {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The state offset of the directory's address.
    pub directory: u32,
    /// The state offset of the shift, an `i64`.
    pub shift: u32,
    /// The state offset of the tag table's address.
    pub tags: u32,
}

/// The state layout of a sink.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Rows out.
    Result {
        /// The state offset of the row count, an `i64`.
        count: u32,
        /// The output columns.
        columns: Vec<Slot>,
        /// Past a probe, the state offset of how many rows the buffers hold, an `i64`.
        capacity: Option<u32>,
    },
    /// A hash aggregate.
    Aggregate(Grouping),
    /// A join build.
    Build(Building),
    /// Nothing, past a probe that marks the entries it matches.
    Mark,
}

/// A join build's table and the record the body builds for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Building {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The record's shape.
    pub layout: JoinLayout,
    /// The state offset of the record buffer.
    pub record: u32,
}

/// One output column of a result sink.
#[derive(Clone, Debug, PartialEq)]
pub struct Slot {
    /// The state offset of the address of the values buffer.
    pub values: u32,
    /// The state offset of the address of the validity buffer, one byte per row.
    pub valid: u32,
    /// The type the values are written at.
    pub ty: Ty,
    /// The column's type.
    pub logical: Type,
}

/// A hash aggregate's table and accumulators.
#[derive(Clone, Debug, PartialEq)]
pub struct Grouping {
    /// The handle of the table in the query's runtime.
    pub table: u64,
    /// The key columns and their types.
    pub keys: Vec<(KeyField, Type)>,
    /// Where the accumulators start in a row.
    pub acc_offset: u32,
    /// The accumulators, in the order of the aggregate calls.
    pub accs: Vec<Acc>,
    /// For an aggregate with no groups, the state offset where the driver writes the address of
    /// the one row.
    pub row: Option<u32>,
    /// For an aggregate grouped by keys that are all fixed width, the state offset of the row the
    /// last key went to, zero before the first. A row whose key is the last one's goes to the same
    /// row without hashing or probing, which is most rows of a file sorted on its keys.
    pub last: Option<u32>,
    /// For the same aggregates, the state offset of six words `ht_insert` publishes the table in,
    /// which the body probes itself before it calls: the address of the slots, their mask, the
    /// address of the rows by group id, how many keys it found that way, and while the table makes
    /// rows without looking, the address of its lanes' tails and how many more rows the body may
    /// append to them itself.
    pub probe: Option<u32>,
    /// For an aggregate grouped by columns that are all in [`Body::domains`], the state offset of
    /// an array of rows indexed by the keys' indexes, zero for a key not seen yet. It has one more
    /// entry past the last key, which a key with a value not in the domains goes to and which is
    /// always zero, so such a key always takes the way through the hash.
    pub dense: Option<u32>,
    /// For an aggregate by one fixed width key that arrives in runs, the state offset of two
    /// words: the address of the next row of the table's last page and the address past it. A
    /// new run takes that row without hashing, and the table has no slots.
    pub runs: Option<u32>,
    /// Whether those runs were promised ascending, which the driver checks a morsel at a time.
    pub ascending: bool,
    /// The size of a row, which a new run moves past.
    pub row_size: u32,
}

/// One accumulator in a group row.
#[derive(Clone, Debug, PartialEq)]
pub struct Acc {
    /// Where it is, from the start of the accumulators.
    pub offset: u32,
    /// What it keeps.
    pub op: AccOp,
    /// The argument's type, or the result's for `count_star`.
    pub arg: Type,
    /// The result's type.
    pub ty: Type,
}

/// What an accumulator keeps and how the driver finishes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccOp {
    /// An `i64` row count.
    CountStar,
    /// An `i64` count of valid values.
    Count,
    /// An `f64` total and a seen byte after it, for a `sum` of `float8`.
    SumFloat,
    /// An `i128` total, an `i64` part of it after that and a seen byte, for a sum over values of
    /// at most 64 bits. A row adds to the part, and only an add that would overflow it moves the
    /// part into the total, so the total is the sum of both.
    SumSplit,
    /// The same total and part, and an `i64` count after them, for an average over the same.
    AvgSplit,
    /// An `i64` total and a seen byte after it, for a sum over values of at most sixteen bits,
    /// which no group can overflow before it holds 2^48 rows.
    SumNarrow,
    /// An `i64` total and an `i64` count after it, for an average over the same.
    AvgNarrow,
    /// The least value at its width and a seen byte after it.
    Min,
    /// The greatest value at its width and a seen byte after it.
    Max,
    /// The first valid value at its width and a seen byte after it. A string is promoted to the
    /// runtime heap first.
    AnyValue,
    /// The least string, a `str16` and a seen byte, kept by `agg_min_str`.
    MinStr,
    /// The greatest string, kept by `agg_max_str`.
    MaxStr,
    /// A count of distinct values kept in the distinct sets behind the handle. No bytes in the row.
    /// For a number that fits in 64 bits, the state offset of two words the body appends each
    /// value and its group through: where the next pair goes and where the room ends. The runtime
    /// gives it room with `agg_distinct_room` and the driver takes in what is left after a chunk.
    Distinct(u64, Option<u32>),
}

/// Generates a function for every pipeline of the graph.
///
/// # Errors
///
/// A refusal when a pipeline needs something the generator does not know yet, or when what it
/// made does not verify, which is a bug and is refused rather than run.
pub fn generate(graph: &Graph, rt: &mut Rt) -> Result<Query> {
    generate_with(graph, rt, &[])
}

/// What the statistics say about one column a pipeline scans.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    /// Every value of a text column, when it has few of them.
    pub values: Option<Vec<Vec<u8>>>,
    /// Whether most rows of a column that is not text are coded into a dictionary.
    pub coded: bool,
}

/// [`generate`], with what the statistics say about the columns each pipeline scans: one entry
/// per stage, and in each one per source column. A stage past the end of `known` knows nothing.
///
/// # Errors
///
/// As for [`generate`].
pub fn generate_with(graph: &Graph, rt: &mut Rt, known: &[Vec<Known>]) -> Result<Query> {
    let mut module = Module::new("query");
    let mut bodies: Vec<Option<Body>> = Vec::with_capacity(graph.stages.len());
    for (stage, s) in graph.stages.iter().enumerate() {
        bodies.push(match s {
            Stage::Pipeline(p) => {
                let mut joins = Vec::new();
                for probe in p.probes() {
                    match bodies.get(probe.build).and_then(Option::as_ref).map(|b| &b.sink) {
                        Some(Out::Build(b)) => joins.push(b.clone()),
                        _ => {
                            return Err(Refusal::new(
                                "HashJoin",
                                "a probe of a stage that is not an earlier build",
                            ));
                        }
                    }
                }
                let known = known.get(stage).map_or(&[][..], Vec::as_slice);
                Some(pipeline(stage, p, &joins, &mut module, rt, known)?)
            }
            _ => None,
        });
    }
    if let Err(errors) = verify(&module) {
        let why = errors.iter().map(|e| format!("{} {}", e.rule, e.message)).collect::<Vec<_>>();
        return Err(Refusal::new("the generated code", why.join("; ")));
    }
    Ok(Query { module, bodies })
}

/// The QIR type of a value of type `ty`.
///
/// # Errors
///
/// For a type with no one QIR type.
pub fn qir_type(ty: Type) -> Result<Ty> {
    ty.data().ok_or_else(|| Refusal::new(format!("type {ty}"), "the type has no QIR type"))
}

/// Whether values of `ty` order and widen as unsigned numbers: `bool`, `"char"`, the OID types, `xid8`, `pg_lsn` and `uuid`.
#[must_use]
pub fn unsigned(ty: Type) -> bool {
    matches!(
        ty.oid,
        oid::BOOL
            | oid::CHAR
            | oid::OID
            | oid::REGPROC
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::XID
            | oid::CID
            | oid::XID8
            | oid::PG_LSN
            | oid::UUID
    )
}

/// Whether `ty` is `text` or `varchar`, the strings the generator orders and matches by their bytes.
fn is_text(ty: Type) -> bool {
    matches!(ty.oid, oid::TEXT | oid::VARCHAR)
}

/// Whether `min` and `max` of `ty` order its values as signed numbers, as PostgreSQL does.
fn ordered(ty: Type) -> bool {
    matches!(
        ty.oid,
        oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::DATE
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::MONEY
    )
}

/// The message of PostgreSQL for a result out of the range of the integer type `ty`.
fn out_of_range(ty: Type) -> Option<&'static str> {
    Some(match ty.oid {
        oid::INT2 => "smallint out of range",
        oid::INT4 => "integer out of range",
        oid::INT8 => "bigint out of range",
        _ => return None,
    })
}

/// The operation of the integer or float arithmetic function with the `prosrc` `src`, such as `int4pl`, `int48mul`, `float8div` or `int2um`, and whether it is on floats.
fn arithmetic(src: &str) -> Option<(&str, bool)> {
    let (rest, float) = match src.strip_prefix("int") {
        Some(rest) => (rest, false),
        None => (src.strip_prefix("float")?, true),
    };
    let at = rest.find(|c: char| !c.is_ascii_digit())?;
    let (widths, op) = rest.split_at(at);
    let widths = if float {
        matches!(widths, "4" | "8" | "48" | "84")
    } else {
        matches!(widths, "2" | "4" | "8" | "24" | "42" | "28" | "82" | "48" | "84")
    };
    let mod_ok = !float || op != "mod";
    (widths && mod_ok && matches!(op, "pl" | "mi" | "mul" | "div" | "mod" | "um" | "up"))
        .then_some((op, float))
}

/// Whether `e` calls a function, which can raise an error.
fn calls(e: &Expr) -> bool {
    let mut any = matches!(e.kind, Kind::Function { .. });
    e.children(|c| any |= calls(c));
    any
}

fn pipeline(
    stage: usize,
    p: &Pipeline,
    joins: &[Building],
    module: &mut Module,
    rt: &mut Rt,
    known: &[Known],
) -> Result<Body> {
    let name = format!("p{stage}");
    let source = p.source.columns();
    // Only the first operator sees every row of the morsel. A `LIKE` past a filter is asked only of
    // the rows that got through, and answering it for the whole morsel would ask all of them.
    let mut likes: Vec<Matched> = Vec::new();
    if let Some(PipeOp::Filter(f)) = p.ops.first()
        && !rt.ablate().off(Ablate::LIKE)
    {
        matched(f, source.len(), &mut likes);
    }
    for m in &mut likes {
        m.like = match m.order {
            Some(keep) => rt.add_ordered(&m.pattern, keep),
            None => rt.add_like(&m.pattern, false),
        };
    }
    let mut reads = Vec::new();
    // Only the source's columns are read from the morsel. The rest are what the probes bring, and
    // a column only an answered `LIKE` reads is not read itself.
    let mut note = |e: &Expr| read_by(e, source.len(), &likes, &mut reads);
    for op in &p.ops {
        match op {
            PipeOp::Filter(f) => note(f),
            PipeOp::Probe(probe) => {
                probe.keys.iter().chain(&probe.residual).for_each(&mut note);
            }
        }
    }
    match &p.sink {
        Sink::Result { exprs, .. } => exprs.iter().for_each(&mut note),
        Sink::Build { keys, payload, .. } => {
            keys.iter().for_each(&mut note);
            payload.iter().for_each(&mut note);
        }
        Sink::Aggregate { groups, aggregates, .. } => {
            groups.iter().for_each(&mut note);
            for a in aggregates {
                a.args.iter().for_each(&mut note);
                if let Some(f) = &a.filter {
                    note(f);
                }
            }
        }
        Sink::Mark => {}
    }
    reads.sort_unstable();
    let domains = domains(p, known);
    let keyed = keyed(p, known, &domains, rt.ablate());
    let generic = Pass {
        name: &name,
        version: "generic",
        nonull: false,
        replay: None,
        known,
        domains: &domains,
        keyed,
    };
    let (func, out, state, probes, made) =
        emit(stage, p, joins, module, rt, &reads, &likes, generic)?;
    module.funcs.push(func);
    // The version for a morsel with no NULL in the columns it reads. It shares the generic one's
    // tables and kernels and has to come out with the same state, or it is left out.
    let mut nonull = None;
    if !reads.is_empty() || !likes.is_empty() {
        let variant = format!("{name}_nonull");
        let pass = Pass {
            name: &variant,
            version: "nonull",
            nonull: true,
            replay: Some(&made),
            known,
            domains: &domains,
            keyed,
        };
        if let Ok((f, o, st, pr, _)) = emit(stage, p, joins, module, rt, &reads, &likes, pass)
            && (&o, st, &pr) == (&out, state, &probes)
        {
            module.funcs.push(f);
            module.guard("no NULL in the columns the morsel reads", &name, true);
            nonull = Some(variant);
        }
    }
    let coded = reads.iter().copied().filter(|&c| known.get(c).is_some_and(|k| k.coded)).collect();
    Ok(Body { func: name, reads, state, sink: out, probes, nonull, likes, domains, coded, keyed })
}

/// The one group key column of `p` whose codes can index an array of group rows: a text column,
/// which is always read through codes, or one the statistics say is mostly coded. Not when the
/// key is in [`Body::domains`], whose array does the same from the statistics' values.
fn keyed(p: &Pipeline, known: &[Known], domains: &[Domain], ablate: Ablate) -> Option<usize> {
    let Sink::Aggregate { groups, .. } = &p.sink else { return None };
    let [key] = groups.as_slice() else { return None };
    let Kind::Column(c) = key.kind else { return None };
    // A column a probe brings is not in the morsel, and has no codes.
    if c >= p.source.columns().len() {
        return None;
    }
    let text = qir_type(key.ty).is_ok_and(|ty| ty == Ty::Str16);
    let coded = text || known.get(c).is_some_and(|k| k.coded);
    (coded && domains.is_empty() && !ablate.off(Ablate::KEYED)).then_some(c)
}

/// The most entries the array of [`Grouping::dense`] has.
const DENSE: u64 = 1024;

/// The group key columns of `p`, when its sink is an aggregate grouped by source columns whose
/// values the statistics all have and few enough of them for an array indexed by every key there
/// can be. That is the perfect hash on dictionaries of section 11.4 of
/// `spec/compiler/11-memory-and-data-structures.md`.
fn domains(p: &Pipeline, known: &[Known]) -> Vec<Domain> {
    let Sink::Aggregate { groups, .. } = &p.sink else { return Vec::new() };
    let mut domains = Vec::with_capacity(groups.len());
    let mut keys = 1u64;
    for e in groups {
        let Kind::Column(c) = e.kind else { return Vec::new() };
        let Some(values) = known.get(c).and_then(|k| k.values.as_ref()) else {
            return Vec::new();
        };
        keys = keys.saturating_mul(values.len() as u64 + 1);
        domains.push(Domain { column: c, values: values.clone() });
    }
    if keys >= DENSE {
        return Vec::new();
    }
    domains
}

/// Adds every source column `e` reads outside the `LIKE`s in `likes` to `reads`, each once.
fn read_by(e: &Expr, sources: usize, likes: &[Matched], reads: &mut Vec<usize>) {
    match &e.kind {
        Kind::Column(c) if *c < sources && !reads.contains(c) => reads.push(*c),
        Kind::Function { .. } | Kind::Compare { .. } if answered(e, likes).is_some() => {}
        _ => e.children(|c| read_by(c, sources, likes, reads)),
    }
}

/// The index in `likes` of the `LIKE` or comparison that `e` is, if it is one of them.
fn answered(e: &Expr, likes: &[Matched]) -> Option<usize> {
    let (column, text, order) = asked(e)?;
    likes.iter().position(|m| (m.column, m.pattern.as_str(), m.order) == (column, text, order))
}

/// The text of a constant of `text` or `varchar`.
fn text_constant(e: &Expr) -> Option<&str> {
    match &e.kind {
        Kind::Constant(Value::Text(p)) if is_text(e.ty) => Some(p.as_str()),
        _ => None,
    }
}

/// The pattern of a `LIKE` whose pattern is a constant with no backslash, which PostgreSQL takes as its escape character and the runtime does not.
fn like_pattern(e: &Expr) -> Option<&str> {
    text_constant(e).filter(|p| !p.contains('\\'))
}

/// The column, the constant and for a comparison the orders that make it true, of a `LIKE` or `NOT LIKE` of a `text` column against a constant or a comparison of one with a constant.
///
/// An order against a constant is asked of the whole morsel like a `LIKE` is, because a row at a time it is a runtime call a row, and a column coded into a dictionary then answers it once a value. An equality is left to the body, which asks it inline: on ClickBench, answering `<> ''` for the morsel saved up to 8% of the cycles where the column came coded and cost as much where it came flat.
fn asked(e: &Expr) -> Option<(usize, &str, Option<[bool; 3]>)> {
    let text = |s: &Expr| match s.kind {
        Kind::Column(c) if is_text(s.ty) => Some(c),
        _ => None,
    };
    match &e.kind {
        Kind::Function { src: "textlike" | "textnlike", args, .. } => {
            let [s, pattern] = args.as_slice() else { return None };
            Some((text(s)?, like_pattern(pattern)?, None))
        }
        Kind::Compare { op, left, right } => {
            let keep = match op {
                CompareOp::Less => [true, false, false],
                CompareOp::LessEqual => [true, true, false],
                CompareOp::Greater => [false, false, true],
                CompareOp::GreaterEqual => [false, true, true],
                CompareOp::Equal | CompareOp::NotEqual => return None,
            };
            match (text(left), text_constant(right), text(right), text_constant(left)) {
                (Some(c), Some(p), ..) => Some((c, p, Some(keep))),
                // The constant on the left orders the other way round.
                (_, _, Some(c), Some(p)) => Some((c, p, Some([keep[2], keep[1], keep[0]]))),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Adds every `LIKE` in `e` of a source column against a constant, and every order of one against a constant, to `likes`, each once.
fn matched(e: &Expr, sources: usize, likes: &mut Vec<Matched>) {
    if let Some((column, text, order)) = asked(e)
        && column < sources
    {
        if answered(e, likes).is_none() {
            likes.push(Matched { column, pattern: text.to_owned(), order, like: 0 });
        }
        return;
    }
    e.children(|c| matched(c, sources, likes));
}

/// One pass over a pipeline: the function's name and version, whether it reads every column as
/// valid, and on the second pass the handles the first one made.
#[derive(Clone, Copy)]
struct Pass<'a> {
    name: &'a str,
    version: &'a str,
    nonull: bool,
    replay: Option<&'a [u64]>,
    /// What the statistics say about the source columns.
    known: &'a [Known],
    /// The group key columns the body reads as indexes.
    domains: &'a [Domain],
    /// The group key column whose codes index the array of [`Body::keyed`].
    keyed: Option<usize>,
}

/// A pipeline's function, its sink, the bytes of state it needs, its probes and the handles it made
/// in the runtime.
type Emitted = (Func, Out, u32, Vec<Probing>, Vec<u64>);

#[allow(clippy::too_many_arguments)]
fn emit(
    stage: usize,
    p: &Pipeline,
    joins: &[Building],
    module: &mut Module,
    rt: &mut Rt,
    reads: &[usize],
    likes: &[Matched],
    pass: Pass<'_>,
) -> Result<Emitted> {
    let source = p.source.columns();
    let mut g = Gen {
        b: Builder::new(pass.name, pass.version, stage as u32),
        module,
        rt,
        cols: BTreeMap::new(),
        likes: Vec::new(),
        matched: Vec::new(),
        loaded: BTreeMap::new(),
        row: Val::NONE,
        ptrs: Vec::new(),
        next: 0,
        joins: joins.to_vec(),
        tables: Vec::new(),
        nonull: pass.nonull,
        made: Vec::new(),
        replay: pass.replay.map(|made| (made, 0)),
        known: pass.known,
        dense: Vec::new(),
        keyed: None,
    };
    g.b.func_mut().state.push(Field { offset: 0, size: HEADER, name: "header".into() });

    // The entry block reads the morsel and the column table once.
    let m = g.b.m();
    let begin = g.b.load(Ty::I32, m, Val::NONE, 1, MORSEL_BEGIN, INV);
    let end = g.b.load(Ty::I32, m, Val::NONE, 1, MORSEL_END, INV);
    let begin = g.b.conv(Op::Zext, begin, Ty::I64);
    let end = g.b.conv(Op::Zext, end, Ty::I64);
    let table = g.b.load(Ty::Ptr, m, Val::NONE, 1, MORSEL_COLS, INV);
    for (j, &c) in reads.iter().enumerate() {
        let at = j as i32 * COL_SIZE;
        let values = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        let valid = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_VALID, INV);
        let ty = qir_type(source[c].ty)?;
        let coded = ty == Ty::Str16 || g.known.get(c).is_some_and(|k| k.coded);
        let codes = coded.then(|| g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_CODES, INV));
        g.cols.insert(c, (values, valid, ty, codes));
    }
    for k in 0..likes.len() {
        let at = (reads.len() + k) as i32 * COL_SIZE;
        let values = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        let valid = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_VALID, INV);
        let codes = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_CODES, INV);
        g.likes.push((values, valid, codes));
    }
    g.matched = likes.to_vec();
    for (k, d) in pass.domains.iter().enumerate() {
        let at = (reads.len() + likes.len() + k) as i32 * COL_SIZE;
        let values = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        let codes = g.b.load(Ty::Ptr, table, Val::NONE, 1, at + COL_CODES, INV);
        g.dense.push((values, codes, d.values.len() as u32));
    }
    if let Some(c) = pass.keyed
        && let Some(&(_, _, _, Some(codes))) = g.cols.get(&c)
    {
        let at = (reads.len() + likes.len() + pass.domains.len()) as i32 * COL_SIZE;
        let rows = g.b.load(Ty::Ptr, table, Val::NONE, 1, at, INV);
        g.keyed = Some((rows, codes));
    }
    let fans_out = p.probes().next().is_some();
    let (out, state) = g.prepare_sink(&p.sink, fans_out)?;
    g.next = state;
    let st = g.b.st();
    let mut probes = Vec::with_capacity(joins.len());
    for (k, j) in joins.iter().enumerate() {
        let at = g.next.next_multiple_of(8);
        g.field(at, 8, &format!("probe{k}.directory"));
        g.field(at + 8, 8, &format!("probe{k}.shift"));
        g.field(at + 16, 8, &format!("probe{k}.tags"));
        g.next = at + 24;
        let directory = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, INV);
        let shift = g.b.load(Ty::I64, st, Val::NONE, 1, at as i32 + 8, INV);
        let tags = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32 + 16, INV);
        g.tables.push((directory, shift, tags));
        probes.push(Probing { table: j.table, directory: at, shift: at + 8, tags: at + 16 });
    }
    match &out {
        Out::Result { columns, .. } => {
            for slot in columns {
                let values = g.b.load(Ty::Ptr, st, Val::NONE, 1, slot.values as i32, INV);
                let valid = g.b.load(Ty::Ptr, st, Val::NONE, 1, slot.valid as i32, INV);
                g.ptrs.extend([values, valid]);
            }
        }
        Out::Aggregate(grouping) => {
            if let Some(at) = grouping.row {
                let row = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, INV);
                g.ptrs.push(row);
            }
        }
        Out::Build(_) | Out::Mark => {}
    }

    let head = g.b.block(&[(Ty::I64, "i")]);
    let body = g.b.block(&[]);
    let next = g.b.block(&[]);
    let exit = g.b.block(&[]);
    g.b.br(head, &[begin]);

    g.b.switch_to(head);
    g.b.set_loop(head, 1);
    let i = g.b.param(head, 0);
    g.row = i;
    g.b.poll(1024);
    let more = g.b.bin(Op::IcmpSlt, i, end);
    g.b.brif(more, body, &[], exit, &[]);

    g.b.switch_to(body);
    g.ops(&p.ops, 0, next, 1, &p.sink, &out)?;

    g.b.switch_to(next);
    let one = g.b.int(Ty::I64, 1);
    let i1 = g.b.bin(Op::Add, i, one);
    g.b.br(head, &[i1]);

    g.b.switch_to(exit);
    let ok = g.b.int(Ty::I64, 0);
    g.b.ret(ok);

    if let Some((made, used)) = g.replay
        && used != made.len()
    {
        return Err(Refusal::new("the second version of a pipeline", "it made fewer handles"));
    }
    let state = g.next.next_multiple_of(8);
    let made = std::mem::take(&mut g.made);
    let mut func = g.b.finish();
    dce(&mut func);
    Ok((func, out, state, probes, made))
}

struct Gen<'a> {
    b: Builder,
    module: &'a mut Module,
    rt: &'a mut Rt,
    /// Source column to its values address, validity address, type, and the address of its codes
    /// for a text column, which is read through them.
    cols: BTreeMap<usize, (Val, Val, Ty, Option<Val>)>,
    /// The `LIKE`s the driver answers, and the address of each one's answers, of its column's
    /// validity and of the codes the answers are read through.
    matched: Vec<Matched>,
    likes: Vec<(Val, Val, Val)>,
    /// Source columns already read for the current row, in a block every later use is dominated
    /// by.
    loaded: BTreeMap<usize, (Val, Val)>,
    /// The row number.
    row: Val,
    /// The sink's buffer addresses, read once in the entry block: a result's values and validity
    /// per column, or an aggregate's one row.
    ptrs: Vec<Val>,
    /// Where the next field goes in the state, past the sink's and the probes'.
    next: u32,
    /// The builds of the tables the probes read, in the order of the probes.
    joins: Vec<Building>,
    /// Per probe, the directory's address, the shift and the tag table's address, read once in
    /// the entry block.
    tables: Vec<(Val, Val, Val)>,
    /// Whether every column the body reads is taken as valid, for the `nonull` version.
    nonull: bool,
    /// The handles this pass made in the runtime, in order.
    made: Vec<u64>,
    /// On the second pass, the first one's handles and how many of them are handed out so far.
    replay: Option<(&'a [u64], usize)>,
    /// What the statistics say about the source columns.
    known: &'a [Known],
    /// Per group key column read as an index, the address of the indexes, of the codes they are
    /// read through, and how many values the column has.
    dense: Vec<(Val, Val, u32)>,
    /// The address of the array of [`Body::keyed`] and of the codes that index it.
    keyed: Option<(Val, Val)>,
}

/// A value and whether it is valid.
type Pair = (Val, Val);

impl Gen<'_> {
    fn field(&mut self, offset: u32, size: u32, name: &str) {
        self.b.func_mut().state.push(Field { offset, size, name: name.into() });
    }

    /// Lays out the sink's state and makes its table, and returns the layout and the state size.
    fn prepare_sink(&mut self, sink: &Sink, fans_out: bool) -> Result<(Out, u32)> {
        match sink {
            Sink::Result { exprs, columns } => {
                self.field(SINK, 8, "count");
                let mut slots = Vec::with_capacity(exprs.len());
                for (k, (e, c)) in exprs.iter().zip(columns).enumerate() {
                    let values = SINK + 8 + 16 * k as u32;
                    self.field(values, 8, &format!("out{k}.values"));
                    self.field(values + 8, 8, &format!("out{k}.valid"));
                    slots.push(Slot {
                        values,
                        valid: values + 8,
                        ty: qir_type(e.ty)?,
                        logical: c.ty,
                    });
                }
                let mut state = SINK + 8 + 16 * exprs.len() as u32;
                let capacity = fans_out.then(|| {
                    self.field(state, 8, "capacity");
                    state += 8;
                    state - 8
                });
                Ok((Out::Result { count: SINK, columns: slots, capacity }, state))
            }
            Sink::Mark => Ok((Out::Mark, SINK)),
            Sink::Build { keys, payload, marked, .. } => {
                let mut size = 0u32;
                let mut field = |e: &Expr, what: &str| -> Result<KeyField> {
                    let ty = qir_type(e.ty)?;
                    if ty.is_float() && what == "key" {
                        return Err(Refusal::new(
                            "HashJoin",
                            "a floating point key, which the table would compare by its bits",
                        ));
                    }
                    if e.ty.oid == oid::BPCHAR && what == "key" {
                        return Err(Refusal::new(
                            "HashJoin",
                            "a bpchar key, which compares without its trailing spaces",
                        ));
                    }
                    let f = KeyField { offset: size, width: ty.bytes(), text: ty == Ty::Str16 };
                    size += f.width + 1;
                    Ok(f)
                };
                let keys = keys.iter().map(|e| field(e, "key")).collect::<Result<Vec<_>>>()?;
                let payload =
                    payload.iter().map(|e| field(e, "payload")).collect::<Result<Vec<_>>>()?;
                let layout = JoinLayout { keys, payload, size, marked: marked.is_some() };
                let table = self.once(|g| g.rt.add_join(JoinTable::new(layout.clone())))?;
                let buffer = size.next_multiple_of(8);
                self.field(SINK, buffer, "record");
                Ok((Out::Build(Building { table, layout, record: SINK }), SINK + buffer))
            }
            Sink::Aggregate { groups, aggregates, runs, .. } => {
                let mut keys = Vec::with_capacity(groups.len());
                let mut size = 0u32;
                for e in groups {
                    let ty = qir_type(e.ty)?;
                    if ty.is_float() {
                        return Err(Refusal::new(
                            "grouping by a floating point value",
                            "the table compares keys as bytes, and 0.0 and -0.0 are one group",
                        ));
                    }
                    if e.ty.oid == oid::BPCHAR {
                        return Err(Refusal::new(
                            "grouping by a bpchar",
                            "the table compares keys as bytes, and a bpchar without its trailing spaces",
                        ));
                    }
                    let width = ty.bytes();
                    keys.push((KeyField { offset: size, width, text: ty == Ty::Str16 }, e.ty));
                    size += width + 1;
                }
                let mut accs = Vec::with_capacity(aggregates.len());
                let mut at = 0u32;
                for a in aggregates {
                    let acc = self.accumulator(a, at)?;
                    at += acc_size(&acc)?;
                    accs.push(acc);
                }
                let layout = Layout {
                    keys: keys.iter().map(|(k, _)| *k).collect(),
                    key_size: size,
                    init: vec![0; at as usize],
                };
                let acc_offset = Layout::acc_offset(size);
                let row_size = layout.row_size() as u32;
                let table = self.once(|g| g.rt.add_table(GroupTable::new(layout)))?;
                let (row, last, state) = if groups.is_empty() {
                    self.field(SINK, 8, "row");
                    (Some(SINK), None, SINK + 8)
                } else {
                    let size = size.next_multiple_of(8);
                    self.field(SINK, size, "key");
                    self.field(SINK + size, 8, "last");
                    self.field(SINK + size + 8, 48, "probe");
                    (None, Some(SINK + size), SINK + size + 56)
                };
                // A key that arrives in runs closes its groups as it moves on, which needs one key
                // the row can hold without the heap, and no distinct set, whose pairs are kept by
                // group id.
                let closes = runs.filter(|_| {
                    keys.len() == 1
                        && !keys[0].0.text
                        && self.dense.is_empty()
                        && !accs.iter().any(|a| matches!(a.op, AccOp::Distinct(..)))
                        && !self.rt.ablate().off(Ablate::RUNS)
                });
                let probe = last
                    .filter(|_| closes.is_none() && !self.rt.ablate().off(Ablate::PROBE))
                    .map(|at| at + 8);
                let (dense, state) = if groups.is_empty() || self.dense.is_empty() {
                    (None, state)
                } else {
                    let slots = self.dense.iter().map(|&(_, _, n)| n + 1).product::<u32>() + 1;
                    self.field(state, slots * 8, "dense");
                    (Some(state), state + slots * 8)
                };
                let mut state = state;
                for acc in &mut accs {
                    if let AccOp::Distinct(_, words) = &mut acc.op
                        && appends(acc.arg)?
                        && !self.rt.ablate().off(Ablate::PAIRS)
                    {
                        self.field(state, 16, "pairs");
                        *words = Some(state);
                        state += 16;
                    }
                }
                let (at, state) = match closes {
                    Some(_) => {
                        self.field(state, 16, "runs");
                        (Some(state), state + 16)
                    }
                    None => (None, state),
                };
                if at.is_some()
                    && let Some(t) = self.rt.table_mut(table)
                {
                    t.runs();
                }
                let grouping = Grouping {
                    table,
                    keys,
                    acc_offset,
                    accs,
                    row,
                    last,
                    probe,
                    dense,
                    runs: at,
                    ascending: closes == Some(Runs::Ascending),
                    row_size,
                };
                Ok((Out::Aggregate(grouping), state))
            }
        }
    }

    fn accumulator(&mut self, a: &Aggregate, offset: u32) -> Result<Acc> {
        let arg = a.args.first().map_or(a.ty, |e| e.ty);
        let argty = qir_type(arg)?;
        let refuse = || {
            Refusal::new(format!("{}({arg})", a.name), "the generator has no accumulator for it")
        };
        let integer = matches!(arg.oid, oid::INT2 | oid::INT4 | oid::INT8);
        let op = match (a.name, a.distinct) {
            ("count_star", _) => AccOp::CountStar,
            // Floats that PostgreSQL takes as equal can differ in their bits, as 0 and -0 do, and a
            // `bpchar` compares without its trailing spaces, so a set of their bits would count
            // them apart.
            ("count", true) => {
                if argty.is_float() || arg.oid == oid::BPCHAR {
                    return Err(refuse());
                }
                AccOp::Distinct(self.once(|g| g.rt.add_distinct())?, None)
            }
            ("count", false) => AccOp::Count,
            // `sum` of an `int2` or `int4` is an `int8`, which the driver checks at the end, and
            // of an `int8` a `numeric`, both from the exact total.
            ("sum", _)
                if integer
                    && ((a.ty.oid == oid::INT8 && arg.oid != oid::INT8)
                        || (a.ty.oid == oid::NUMERIC && arg.oid == oid::INT8)) =>
            {
                if arg.oid == oid::INT2 { AccOp::SumNarrow } else { AccOp::SumSplit }
            }
            // `sum` of a `float4` adds in `float4`, and `avg` of a float keeps the sum of the
            // squares too, which `float8_accum` checks for an overflow, so neither is here.
            ("sum", _) if arg.oid == oid::FLOAT8 && a.ty.oid == oid::FLOAT8 => AccOp::SumFloat,
            // `avg` of an integer is a `numeric`, the exact total over the count.
            ("avg", _) if integer && a.ty.oid == oid::NUMERIC => {
                if arg.oid == oid::INT2 {
                    AccOp::AvgNarrow
                } else {
                    AccOp::AvgSplit
                }
            }
            ("min" | "max", _) if a.ty.oid != arg.oid => return Err(refuse()),
            ("min", _) if is_text(arg) => AccOp::MinStr,
            ("max", _) if is_text(arg) => AccOp::MaxStr,
            ("min", _) if ordered(arg) => AccOp::Min,
            ("max", _) if ordered(arg) => AccOp::Max,
            ("any_value", _) => AccOp::AnyValue,
            _ => return Err(refuse()),
        };
        Ok(Acc { offset, op, arg, ty: a.ty })
    }

    /// The row's answer to the `LIKE` or comparison `at` of [`Gen::matched`], which the driver
    /// made for the whole morsel, and whether its column is valid.
    fn answer(&mut self, at: usize) -> Pair {
        let (answers, valid, codes) = self.likes[at];
        let ok = if self.nonull { self.truth() } else { self.b.load_bit(valid, self.row) };
        let code = self.code(codes);
        let m = self.b.load(Ty::I1, answers, code, 1, 0, 0);
        let no = self.b.bool(false);
        (self.b.select(ok, m, no), ok)
    }

    /// Reads every source column `e` uses that is not read yet, in the current block. In the
    /// `nonull` version every value is valid, and what the builder folds away with that is every
    /// validity check downstream of the read.
    fn load_columns(&mut self, e: &Expr) {
        for c in e.columns() {
            if self.loaded.contains_key(&c) {
                continue;
            }
            // A column only an answered `LIKE` reads is not in the morsel's table.
            let Some(&(values, valid, ty, codes)) = self.cols.get(&c) else { continue };
            let at = codes.map_or(self.row, |codes| self.code(codes));
            let v = self.b.load(ty, values, at, ty.bytes(), 0, 0);
            if self.nonull {
                let ok = self.truth();
                self.loaded.insert(c, (v, ok));
                continue;
            }
            let ok = self.b.load_bit(valid, self.row);
            let zero = self.zero(ty);
            let v = self.b.select(ok, v, zero);
            self.loaded.insert(c, (v, ok));
        }
    }

    /// The row's index into a table the driver hands with `codes`, as an `i64`.
    fn code(&mut self, codes: Val) -> Val {
        let code = self.b.load(Ty::I32, codes, self.row, 4, 0, 0);
        self.b.conv(Op::Zext, code, Ty::I64)
    }

    /// A handle the first pass over a pipeline makes in the runtime with `make`, and the second
    /// hands out again, so that both versions of the function share their tables and kernels.
    fn once(&mut self, make: impl FnOnce(&mut Self) -> u64) -> Result<u64> {
        let Some((made, used)) = self.replay else {
            let h = make(self);
            self.made.push(h);
            return Ok(h);
        };
        self.replay = Some((made, used + 1));
        made.get(used).copied().ok_or_else(|| {
            Refusal::new("the second version of a pipeline", "it made more handles than the first")
        })
    }

    fn zero(&mut self, ty: Ty) -> Val {
        self.b.konst(ty, 0)
    }

    fn truth(&mut self) -> Val {
        self.b.bool(true)
    }

    /// Translates `e` with its columns read first.
    fn expr(&mut self, e: &Expr) -> Result<Pair> {
        self.load_columns(e);
        self.translate(e)
    }

    fn translate(&mut self, e: &Expr) -> Result<Pair> {
        let ty = qir_type(e.ty)?;
        match &e.kind {
            Kind::Column(c) => Ok(self.loaded[c]),
            Kind::Constant(v) => self.constant(v, e.ty, ty),
            Kind::Widen(input) => {
                let (v, ok) = self.translate(input)?;
                if self.b.ty(v) == ty {
                    return Ok((v, ok));
                }
                let op = if ty.is_float() { Op::Fext } else { Op::Sext };
                Ok((self.b.conv(op, v, ty), ok))
            }
            Kind::Compare { op, left, right } => match answered(e, &self.matched) {
                Some(at) => Ok(self.answer(at)),
                None => self.compare(*op, left, right),
            },
            Kind::And(children) | Kind::Or(children) if children.iter().skip(1).any(calls) => {
                self.lazy(children, matches!(e.kind, Kind::Or(_)))
            }
            Kind::And(children) => {
                let (mut all_valid, mut any_false, mut value) =
                    (self.truth(), self.b.bool(false), self.truth());
                for c in children {
                    let (v, ok) = self.translate(c)?;
                    let v = self.b.bin(Op::And, v, ok);
                    let not_v = self.b.un(Op::Not, v);
                    let is_false = self.b.bin(Op::And, ok, not_v);
                    any_false = self.b.bin(Op::Or, any_false, is_false);
                    all_valid = self.b.bin(Op::And, all_valid, ok);
                    value = self.b.bin(Op::And, value, v);
                }
                let valid = self.b.bin(Op::Or, any_false, all_valid);
                let value = self.b.bin(Op::And, value, all_valid);
                Ok((value, valid))
            }
            Kind::Or(children) => {
                let (mut all_valid, mut any_true) = (self.truth(), self.b.bool(false));
                for c in children {
                    let (v, ok) = self.translate(c)?;
                    let v = self.b.bin(Op::And, v, ok);
                    any_true = self.b.bin(Op::Or, any_true, v);
                    all_valid = self.b.bin(Op::And, all_valid, ok);
                }
                let valid = self.b.bin(Op::Or, any_true, all_valid);
                Ok((any_true, valid))
            }
            Kind::Not(input) => {
                let (v, ok) = self.translate(input)?;
                let not = self.b.un(Op::Not, v);
                Ok((self.b.bin(Op::And, not, ok), ok))
            }
            Kind::IsNull { input, negated } => {
                let (_, ok) = self.translate(input)?;
                let v = if *negated { ok } else { self.b.un(Op::Not, ok) };
                Ok((v, self.truth()))
            }
            Kind::Function { src, strict, args, .. } => self.function(src, *strict, args, e),
            Kind::Case { arms, otherwise } => {
                let join = self.b.block(&[(ty, "case"), (Ty::I1, "case.ok")]);
                for (when, then) in arms {
                    let (c, ok) = self.translate(when)?;
                    let c = self.b.bin(Op::And, c, ok);
                    let (yes, no) = (self.b.block(&[]), self.b.block(&[]));
                    self.b.brif(c, yes, &[], no, &[]);
                    self.b.switch_to(yes);
                    let (v, ok) = self.translate(then)?;
                    self.b.br(join, &[v, ok]);
                    self.b.switch_to(no);
                }
                let (v, ok) = match otherwise {
                    Some(o) => self.translate(o)?,
                    None => (self.zero(ty), self.b.bool(false)),
                };
                self.b.br(join, &[v, ok]);
                self.b.switch_to(join);
                Ok((self.b.param(join, 0), self.b.param(join, 1)))
            }
        }
    }

    /// `AND` of `children`, or `OR` when `or`, the way PostgreSQL's executor runs it: a child
    /// runs only when the ones before it did not decide the answer, which a false child does for
    /// an `AND` and a true one for an `OR`, so that a later child that would raise an error for
    /// such a row does not run for it. Past the last child the answer is null when any child was.
    fn lazy(&mut self, children: &[Expr], or: bool) -> Result<Pair> {
        let done = self.b.block(&[(Ty::I1, "bool"), (Ty::I1, "bool.ok")]);
        let decided = self.b.bool(or);
        let yes = self.truth();
        let mut all_valid = self.truth();
        for c in children {
            let (v, ok) = self.translate(c)?;
            let v = if or { v } else { self.b.un(Op::Not, v) };
            let decides = self.b.bin(Op::And, v, ok);
            all_valid = self.b.bin(Op::And, all_valid, ok);
            let next = self.b.block(&[]);
            self.b.brif(decides, done, &[decided, yes], next, &[]);
            self.b.switch_to(next);
        }
        // An `AND` with no false child is true when every child was valid and null when not,
        // whose value is a harmless false, and an `OR` with no true child is false or null.
        let value = if or { self.b.bool(false) } else { all_valid };
        self.b.br(done, &[value, all_valid]);
        self.b.switch_to(done);
        Ok((self.b.param(done, 0), self.b.param(done, 1)))
    }

    fn constant(&mut self, v: &Value, logical: Type, ty: Ty) -> Result<Pair> {
        let refuse = |why: &str| Refusal::new(format!("a constant of type {logical}"), why);
        let (of, bits): (Ty, u128) = match v {
            Value::Null => return Ok((self.zero(ty), self.b.bool(false))),
            Value::Bool(b) => (Ty::I1, u128::from(*b)),
            Value::Char(x) => (Ty::I8, u128::from(*x)),
            Value::Int2(x) => (Ty::I16, *x as u128),
            Value::Int4(x) | Value::Date(x) => (Ty::I32, *x as u128),
            Value::Oid(x) => (Ty::I32, u128::from(*x)),
            Value::Int8(x) | Value::Time(x) | Value::Timestamp(x) | Value::TimestampTz(x) => {
                (Ty::I64, *x as u128)
            }
            Value::Float4(x) => (Ty::F32, u128::from(x.to_bits())),
            Value::Float8(x) => (Ty::F64, u128::from(x.to_bits())),
            Value::Text(_) | Value::Bytea(_) => (Ty::Str16, 0),
            _ => return Err(refuse("the constant is not generated")),
        };
        if of != ty {
            return Err(refuse("the value has another machine type"));
        }
        let bits = match v {
            Value::Text(s) => self.literal(s.as_bytes()),
            Value::Bytea(b) => self.literal(b),
            _ => bits,
        };
        let valid = self.truth();
        Ok((self.b.konst(ty, bits), valid))
    }

    /// A string constant, inline or kept in the runtime's heap for the whole query.
    fn literal(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= text::INLINE { text::make(bytes) } else { self.rt.keep(bytes) }
    }

    fn error(&mut self, kind: ErrorKind, text: String) -> u32 {
        self.module.error(kind, &text)
    }

    fn compare(&mut self, op: CompareOp, left: &Expr, right: &Expr) -> Result<Pair> {
        let ty = qir_type(left.ty)?;
        let refuse = |why: &str| {
            Refusal::new(format!("comparing {} with {}", left.ty, right.ty), why.to_owned())
        };
        if qir_type(right.ty)? != ty {
            return Err(refuse("the two sides have different types"));
        }
        // A `bpchar` compares without its trailing spaces, which its bytes keep.
        if left.ty.oid == oid::BPCHAR || right.ty.oid == oid::BPCHAR {
            return Err(refuse("a bpchar compares without its trailing spaces"));
        }
        let (a, va) = self.translate(left)?;
        let (b, vb) = self.translate(right)?;
        let eq = |g: &mut Gen<'_>| -> Val {
            if ty == Ty::Str16 {
                if is_empty_string(right) || is_empty_string(left) {
                    let s = if is_empty_string(right) { a } else { b };
                    let len = g.b.un(Op::StrLen, s);
                    let zero = g.b.int(Ty::I32, 0);
                    return g.b.bin(Op::IcmpEq, len, zero);
                }
                return g.text_eq(a, b, true);
            }
            let op = if ty.is_float() { Op::FcmpEq } else { Op::IcmpEq };
            g.b.bin(op, a, b)
        };
        let both = self.b.bin(Op::And, va, vb);
        let value = match op {
            CompareOp::Equal => eq(self),
            CompareOp::NotEqual => {
                let e = eq(self);
                self.b.un(Op::Not, e)
            }
            CompareOp::Less
            | CompareOp::LessEqual
            | CompareOp::Greater
            | CompareOp::GreaterEqual => {
                // Greater is Less with the sides swapped, so there are two orders to generate.
                let (x, y) = if matches!(op, CompareOp::Less | CompareOp::LessEqual) {
                    (a, b)
                } else {
                    (b, a)
                };
                let or_equal = matches!(op, CompareOp::LessEqual | CompareOp::GreaterEqual);
                if ty == Ty::Str16 {
                    let c = self.rt(proxy_id("str_cmp"), &[x, y]);
                    let zero = self.b.int(Ty::I32, 0);
                    self.b.bin(if or_equal { Op::IcmpSle } else { Op::IcmpSlt }, c, zero)
                } else if ty.is_float() {
                    self.b.bin(if or_equal { Op::FcmpLe } else { Op::FcmpLt }, x, y)
                } else if unsigned(left.ty) {
                    self.b.bin(if or_equal { Op::IcmpUle } else { Op::IcmpUlt }, x, y)
                } else {
                    self.b.bin(if or_equal { Op::IcmpSle } else { Op::IcmpSlt }, x, y)
                }
            }
        };
        Ok((value, both))
    }

    /// An `rtcall` of a proxy with a result.
    fn rt(&mut self, id: u32, args: &[Val]) -> Val {
        self.b.rtcall(id, args).unwrap_or(Val::NONE)
    }

    fn handle(&mut self, h: u64) -> Val {
        self.b.konst(Ty::Ptr, u128::from(h))
    }

    /// A call of the function with the `prosrc` `src`, translated inline, or refused by its name
    /// and argument types when the generator has no translator for it. A function that is not
    /// strict decides itself what a null argument gives, which none of the translators knows.
    fn function(&mut self, src: &str, strict: bool, args: &[Expr], e: &Expr) -> Result<Pair> {
        let refuse = || {
            let types = args.iter().map(|a| a.ty.to_string()).collect::<Vec<_>>().join(", ");
            Refusal::new(
                format!("{src}({types})"),
                "the generator has no translator for the function",
            )
        };
        if !strict {
            return Err(refuse());
        }
        self.inline(src, args, e)?.ok_or_else(refuse)
    }

    /// Branches to a cold block that raises the error `text` of `kind` when `cond` holds.
    fn fail_if(&mut self, cond: Val, kind: ErrorKind, text: &str) {
        let err = self.error(kind, text.to_owned());
        let (bad, next) = (self.b.block(&[]), self.b.block(&[]));
        self.b.set_cold(bad);
        self.b.brif(cond, bad, &[], next, &[]);
        self.b.switch_to(bad);
        self.b.trap(err);
        self.b.switch_to(next);
    }

    /// Whether the float `x` is infinite.
    fn is_inf(&mut self, x: Val) -> Val {
        let ty = self.b.ty(x);
        let abs = self.b.un(Op::Fabs, x);
        let inf = self.infinity(ty);
        self.b.bin(Op::FcmpEq, abs, inf)
    }

    /// Whether the float `x` is a NaN, which the total order puts above infinity.
    fn is_nan(&mut self, x: Val) -> Val {
        let ty = self.b.ty(x);
        let abs = self.b.un(Op::Fabs, x);
        let inf = self.infinity(ty);
        self.b.bin(Op::FcmpLt, inf, abs)
    }

    fn infinity(&mut self, ty: Ty) -> Val {
        if ty == Ty::F32 { self.b.f32(f32::INFINITY) } else { self.b.f64(f64::INFINITY) }
    }

    /// Whether the float `x` is zero, either one.
    fn is_zero(&mut self, x: Val) -> Val {
        let ty = self.b.ty(x);
        let zero = self.zero(ty);
        self.b.bin(Op::FcmpEq, x, zero)
    }

    /// `v` when `ok`, and `or` when not, so that a null row never reaches a trap with the bits a
    /// null left behind.
    fn gate(&mut self, ok: Val, v: Val, or: i128) -> Val {
        let ty = self.b.ty(v);
        let or = if ty == Ty::F64 {
            self.b.f64(or as f64)
        } else if ty == Ty::F32 {
            self.b.f32(or as f32)
        } else {
            self.b.int(ty, or)
        };
        self.b.select(ok, v, or)
    }

    /// The call translated inline, or `None` when no translator takes it. Every check that can say
    /// no is made before an argument is translated.
    fn inline(&mut self, src: &str, args: &[Expr], e: &Expr) -> Result<Option<Pair>> {
        let ty = qir_type(e.ty)?;
        if let Some((op, float)) = arithmetic(src) {
            return if float {
                self.float(op, args, e.ty, ty)
            } else {
                self.integer(op, args, e.ty, ty)
            };
        }
        Ok(Some(match (src, args) {
            ("i2toi4" | "int28" | "int48", [x]) => {
                let (v, ok) = self.translate(x)?;
                if !self.b.ty(v).is_int() || self.b.ty(v).bytes() >= ty.bytes() {
                    return Ok(None);
                }
                (self.b.conv(Op::Sext, v, ty), ok)
            }
            ("i4toi2" | "int82" | "int84", [x]) => {
                let Some(text) = out_of_range(e.ty) else { return Ok(None) };
                let (v, ok) = self.translate(x)?;
                let from = self.b.ty(v);
                if !from.is_int() || from.bytes() <= ty.bytes() {
                    return Ok(None);
                }
                let narrow = self.b.conv(Op::Trunc, v, ty);
                let back = self.b.conv(Op::Sext, narrow, from);
                let lost = self.b.bin(Op::IcmpNe, back, v);
                let lost = self.b.bin(Op::And, lost, ok);
                self.fail_if(lost, ErrorKind::Overflow, text);
                (narrow, ok)
            }
            ("i2tod" | "i4tod" | "i8tod" | "i2tof" | "i4tof" | "i8tof", [x]) => {
                let (v, ok) = self.translate(x)?;
                if !self.b.ty(v).is_int() || !ty.is_float() {
                    return Ok(None);
                }
                (self.b.conv(Op::Sitof, v, ty), ok)
            }
            ("ftod", [x]) => {
                let (v, ok) = self.translate(x)?;
                if self.b.ty(v) != Ty::F32 || ty != Ty::F64 {
                    return Ok(None);
                }
                (self.b.conv(Op::Fext, v, ty), ok)
            }
            ("dtof", [x]) => {
                let (v, ok) = self.translate(x)?;
                if self.b.ty(v) != Ty::F64 || ty != Ty::F32 {
                    return Ok(None);
                }
                let r = self.b.conv(Op::Ftrunc, v, ty);
                // `float8_to_float4` checks the result as `float4in` does.
                let (r_inf, v_inf) = (self.is_inf(r), self.is_inf(v));
                let fin = self.b.un(Op::Not, v_inf);
                let over = self.b.bin(Op::And, r_inf, fin);
                let over = self.b.bin(Op::And, over, ok);
                self.fail_if(over, ErrorKind::Overflow, "value out of range: overflow");
                let (r_zero, v_zero) = (self.is_zero(r), self.is_zero(v));
                let some = self.b.un(Op::Not, v_zero);
                let under = self.b.bin(Op::And, r_zero, some);
                let under = self.b.bin(Op::And, under, ok);
                self.fail_if(under, ErrorKind::Overflow, "value out of range: underflow");
                (r, ok)
            }
            ("dtoi2" | "dtoi4" | "dtoi8" | "ftoi2" | "ftoi4" | "ftoi8", [x]) => {
                let Some(text) = out_of_range(e.ty) else { return Ok(None) };
                let (v, ok) = self.translate(x)?;
                if !self.b.ty(v).is_float() {
                    return Ok(None);
                }
                // `rint` and then the range check, both of which `ftosi.t` makes.
                let v = self.gate(ok, v, 0);
                let err = self.error(ErrorKind::Overflow, text.to_owned());
                (self.b.ftosi(v, ty, err), ok)
            }
            ("textlike" | "textnlike", [s, pattern]) if is_text(s.ty) => {
                let negate = src == "textnlike";
                if let Some(at) = answered(e, &self.matched) {
                    let (m, ok) = self.answer(at);
                    let m = if negate { self.b.un(Op::Not, m) } else { m };
                    let m = self.b.bin(Op::And, m, ok);
                    return Ok(Some((m, ok)));
                }
                let Some(p) = like_pattern(pattern) else { return Ok(None) };
                let p = p.to_owned();
                let h = self.once(|g| g.rt.add_like(&p, false))?;
                let (s, ok) = self.translate(s)?;
                let h = self.handle(h);
                let m = self.rt(proxy_id("str_like"), &[h, s]);
                let m = if negate { self.b.un(Op::Not, m) } else { m };
                (self.b.bin(Op::And, m, ok), ok)
            }
            ("textlen", [s]) if is_text(s.ty) && ty == Ty::I32 => {
                let (s, ok) = self.translate(s)?;
                let n = self.rt(proxy_id("str_length"), &[s]);
                (self.b.conv(Op::Trunc, n, Ty::I32), ok)
            }
            ("textoctetlen", [s]) if is_text(s.ty) && ty == Ty::I32 => {
                let (s, ok) = self.translate(s)?;
                (self.b.un(Op::StrLen, s), ok)
            }
            ("byteaoctetlen", [s]) if s.ty.oid == oid::BYTEA && ty == Ty::I32 => {
                let (s, ok) = self.translate(s)?;
                (self.b.un(Op::StrLen, s), ok)
            }
            ("textcat", [l, r]) if is_text(l.ty) && is_text(r.ty) => {
                let (a, va) = self.translate(l)?;
                let (b, vb) = self.translate(r)?;
                let valid = self.b.bin(Op::And, va, vb);
                (self.rt(proxy_id("str_concat"), &[a, b]), valid)
            }
            _ => return Ok(None),
        }))
    }

    /// `int4pl` and the rest of PostgreSQL's integer arithmetic: the arguments widened to the
    /// result's type, two's complement with a trap where the result leaves it, and a trap on a
    /// zero divisor before the division.
    fn integer(&mut self, op: &str, args: &[Expr], logical: Type, ty: Ty) -> Result<Option<Pair>> {
        let Some(text) = out_of_range(logical) else { return Ok(None) };
        let fits = |a: &Expr| {
            matches!(a.ty.oid, oid::INT2 | oid::INT4 | oid::INT8)
                && qir_type(a.ty).is_ok_and(|t| t.bytes() <= ty.bytes())
        };
        if !args.iter().all(fits) {
            return Ok(None);
        }
        let mut vals = Vec::with_capacity(args.len());
        let mut valid = self.truth();
        for a in args {
            let (v, ok) = self.translate(a)?;
            vals.push(self.widen(v, a.ty, ty));
            valid = self.b.bin(Op::And, valid, ok);
        }
        let range = |g: &mut Gen<'_>| g.error(ErrorKind::Overflow, text.to_owned());
        Ok(Some(match (op, vals.as_slice()) {
            ("up", [a]) => (*a, valid),
            ("um", [a]) => {
                let a = self.gate(valid, *a, 0);
                let err = range(self);
                (self.b.checked_neg(a, err), valid)
            }
            ("pl" | "mi" | "mul", [a, b]) => {
                let (a, b) = (self.gate(valid, *a, 0), self.gate(valid, *b, 0));
                let op = match op {
                    "pl" => Op::SaddT,
                    "mi" => Op::SsubT,
                    _ => Op::SmulT,
                };
                let err = range(self);
                (self.b.checked(op, a, b, err), valid)
            }
            ("div" | "mod", [a, b]) => {
                let (a, b) = (self.gate(valid, *a, 0), self.gate(valid, *b, 1));
                let zero = self.zero(ty);
                let none = self.b.bin(Op::IcmpEq, b, zero);
                if op == "div" {
                    self.fail_if(none, ErrorKind::DivideByZero, "division by zero");
                    let err = range(self);
                    (self.b.checked(Op::SdivT, a, b, err), valid)
                } else {
                    // The remainder of the least value by -1 is 0 in `srem.t`, as in `int4mod`.
                    let err = self.error(ErrorKind::DivideByZero, "division by zero".to_owned());
                    (self.b.checked(Op::SremT, a, b, err), valid)
                }
            }
            _ => return Ok(None),
        }))
    }

    /// `float8pl` and the rest of PostgreSQL's float arithmetic: IEEE arithmetic in the result's
    /// type, and the checks of `float_overflow_error`, `float_underflow_error` and
    /// `float_zero_divide_error`, made only for a row whose arguments are valid.
    fn float(&mut self, op: &str, args: &[Expr], logical: Type, ty: Ty) -> Result<Option<Pair>> {
        if !matches!(logical.oid, oid::FLOAT4 | oid::FLOAT8) {
            return Ok(None);
        }
        let fits = |a: &Expr| match a.ty.oid {
            oid::FLOAT4 => true,
            oid::FLOAT8 => ty == Ty::F64,
            _ => false,
        };
        if !args.iter().all(fits) {
            return Ok(None);
        }
        let mut vals = Vec::with_capacity(args.len());
        let mut valid = self.truth();
        for a in args {
            let (v, ok) = self.translate(a)?;
            let v = if self.b.ty(v) == ty { v } else { self.b.conv(Op::Fext, v, ty) };
            vals.push(v);
            valid = self.b.bin(Op::And, valid, ok);
        }
        Ok(Some(match (op, vals.as_slice()) {
            ("up", [a]) => (*a, valid),
            ("um", [a]) => (self.b.un(Op::Fneg, *a), valid),
            ("pl" | "mi" | "mul" | "div", [a, b]) => {
                let (a, b) = (*a, *b);
                if op == "div" {
                    let (b_zero, a_nan) = (self.is_zero(b), self.is_nan(a));
                    let number = self.b.un(Op::Not, a_nan);
                    let none = self.b.bin(Op::And, b_zero, number);
                    let none = self.b.bin(Op::And, none, valid);
                    self.fail_if(none, ErrorKind::DivideByZero, "division by zero");
                }
                let code = match op {
                    "pl" => Op::Fadd,
                    "mi" => Op::Fsub,
                    "mul" => Op::Fmul,
                    _ => Op::Fdiv,
                };
                let r = self.b.bin(code, a, b);
                let (r_inf, a_inf, b_inf) = (self.is_inf(r), self.is_inf(a), self.is_inf(b));
                let a_fin = self.b.un(Op::Not, a_inf);
                let b_fin = self.b.un(Op::Not, b_inf);
                let mut over = self.b.bin(Op::And, r_inf, a_fin);
                if op != "div" {
                    over = self.b.bin(Op::And, over, b_fin);
                }
                let over = self.b.bin(Op::And, over, valid);
                self.fail_if(over, ErrorKind::Overflow, "value out of range: overflow");
                if matches!(op, "mul" | "div") {
                    let (r_zero, a_zero) = (self.is_zero(r), self.is_zero(a));
                    let a_some = self.b.un(Op::Not, a_zero);
                    let other = if op == "mul" {
                        let b_zero = self.is_zero(b);
                        self.b.un(Op::Not, b_zero)
                    } else {
                        b_fin
                    };
                    let under = self.b.bin(Op::And, r_zero, a_some);
                    let under = self.b.bin(Op::And, under, other);
                    let under = self.b.bin(Op::And, under, valid);
                    self.fail_if(under, ErrorKind::Overflow, "value out of range: underflow");
                }
                (r, valid)
            }
            _ => return Ok(None),
        }))
    }

    /// The address `base + offset`.
    fn offset(&mut self, base: Val, offset: u32) -> Val {
        let x = self.b.conv(Op::Bitcast, base, Ty::I64);
        let k = self.b.int(Ty::I64, i128::from(offset));
        let x = self.b.bin(Op::Add, x, k);
        self.b.conv(Op::Bitcast, x, Ty::Ptr)
    }

    /// Runs `ops` on the current row and then the sink, and goes to `skip` when done with it. The
    /// first probe in `ops` is probe number `probe` of the pipeline, and `depth` is the loop depth
    /// of the code so far.
    fn ops(
        &mut self,
        ops: &[PipeOp],
        probe: usize,
        skip: Block,
        depth: u8,
        sink: &Sink,
        out: &Out,
    ) -> Result<()> {
        let Some((op, rest)) = ops.split_first() else {
            self.sink(sink, out, skip, depth)?;
            self.b.br(skip, &[]);
            return Ok(());
        };
        match op {
            PipeOp::Filter(f) => {
                self.residual(std::slice::from_ref(f), skip)?;
                self.ops(rest, probe, skip, depth, sink, out)
            }
            // Every match that passes the residual gets its mark set, and the row goes no further.
            PipeOp::Probe(p) if p.marks => {
                let (head, e, stride, advance) = self.probe(p, probe, skip, depth + 1)?;
                self.residual(&p.residual, advance)?;
                let entry = self.b.conv(Op::Bitcast, e, Ty::Ptr);
                let at = 8 + self.joins[probe].layout.mark() as i32;
                let one = self.b.int(Ty::I8, 1);
                self.b.store(entry, Val::NONE, 1, at, one, 0);
                self.b.br(advance, &[]);
                self.next_entry(head, e, stride, advance);
                Ok(())
            }
            PipeOp::Probe(p) => match p.kind {
                JoinType::Inner => {
                    let (head, e, stride, advance) = self.probe(p, probe, skip, depth + 1)?;
                    self.ops(rest, probe + 1, advance, depth + 1, sink, out)?;
                    self.next_entry(head, e, stride, advance);
                    Ok(())
                }
                // The first match that passes the residual sends the row on, once, and leaves
                // the loop over the entries for good.
                JoinType::Semi => {
                    let (head, e, stride, advance) = self.probe(p, probe, skip, depth + 1)?;
                    self.residual(&p.residual, advance)?;
                    self.ops(rest, probe + 1, skip, depth, sink, out)?;
                    self.next_entry(head, e, stride, advance);
                    Ok(())
                }
                // A match that passes the residual drops the row, and a row that runs out of
                // entries, or never had any, goes on. What the loop read is not read on that
                // way, so what follows reads it again.
                JoinType::Anti => {
                    let before = self.loaded.clone();
                    let none = self.b.block(&[]);
                    let (head, e, stride, advance) = self.probe(p, probe, none, depth + 1)?;
                    self.residual(&p.residual, advance)?;
                    self.b.br(skip, &[]);
                    self.next_entry(head, e, stride, advance);
                    self.b.switch_to(none);
                    self.loaded = before;
                    self.ops(rest, probe + 1, skip, depth, sink, out)
                }
            },
        }
    }

    /// Ends the block `advance` of a probe's loop by going on to the entry after `e`.
    fn next_entry(&mut self, head: Block, e: Val, stride: u32, advance: Block) {
        self.b.switch_to(advance);
        let stride = self.b.int(Ty::I64, i128::from(stride));
        let e = self.b.bin(Op::Add, e, stride);
        self.b.br(head, &[e]);
    }

    /// Goes to `fail` unless every one of `residual` is true.
    fn residual(&mut self, residual: &[Expr], fail: Block) -> Result<()> {
        for r in residual {
            let (v, ok) = self.expr(r)?;
            let pass = self.b.bin(Op::And, v, ok);
            let then = self.b.block(&[]);
            self.b.brif(pass, then, &[], fail, &[]);
            self.b.switch_to(then);
        }
        Ok(())
    }

    /// The fused probe of section 10.5: hashes the row's keys, tests the tag of the slot the hash
    /// picks, and starts a loop over the slot's entries that leaves the builder in the block of a
    /// match, with the payload read. A row with a null key, or whose tag says the slot cannot hold
    /// it, goes to `skip`. Returns the loop's header, the entry address it carries, the stride
    /// and the block that goes on to the next entry, which the caller ends once the code for a
    /// match is written.
    fn probe(
        &mut self,
        p: &Probe,
        n: usize,
        skip: Block,
        depth: u8,
    ) -> Result<(Block, Val, u32, Block)> {
        let layout = self.joins[n].layout.clone();
        let (directory, shift, tags) = self.tables[n];
        // A null key matches nothing, and the build never stored one.
        let mut hash = self.b.int(Ty::I64, 0);
        let mut keys = Vec::with_capacity(p.keys.len());
        for e in &p.keys {
            let (v, ok) = self.expr(e)?;
            let then = self.b.block(&[]);
            self.b.brif(ok, then, &[], skip, &[]);
            self.b.switch_to(then);
            hash = self.hash(hash, v, e.ty)?;
            keys.push(v);
        }
        let fold = self.b.konst(Ty::I64, u128::from(FOLD));
        let h = self.b.bin(Op::Mul, hash, fold);
        let slot = self.b.bin(Op::Lshr, h, shift);
        let word = self.b.load(Ty::I64, directory, slot, 8, 0, 0);
        let low = self.b.int(Ty::I64, 2047);
        let at = self.b.bin(Op::And, h, low);
        let tag = self.b.load(Ty::I16, tags, at, 2, 0, 0);
        let tag = self.b.conv(Op::Zext, tag, Ty::I64);
        let k48 = self.b.int(Ty::I64, 48);
        let bloom = self.b.bin(Op::Lshr, word, k48);
        let seen = self.b.bin(Op::And, bloom, tag);
        let maybe = self.b.bin(Op::IcmpEq, seen, tag);
        let address = self.b.konst(Ty::I64, u128::from(ADDRESS));
        let lo = self.b.bin(Op::And, word, address);
        let hi = self.b.load(Ty::I64, directory, slot, 8, 8, 0);
        let hi = self.b.bin(Op::And, hi, address);

        let head = self.b.block(&[(Ty::I64, "entry")]);
        self.b.brif(maybe, head, &[lo], skip, &[]);
        self.b.switch_to(head);
        self.b.set_loop(head, depth);
        let e = self.b.param(head, 0);
        self.b.poll(1024);
        let more = self.b.bin(Op::IcmpUlt, e, hi);
        let candidate = self.b.block(&[]);
        self.b.brif(more, candidate, &[], skip, &[]);

        self.b.switch_to(candidate);
        let advance = self.b.block(&[]);
        let entry = self.b.conv(Op::Bitcast, e, Ty::Ptr);
        let stored = self.b.load(Ty::I64, entry, Val::NONE, 1, 0, 0);
        let same = self.b.bin(Op::IcmpEq, stored, hash);
        let check = self.b.block(&[]);
        self.b.brif(same, check, &[], advance, &[]);
        self.b.switch_to(check);
        for ((v, e), f) in keys.into_iter().zip(&p.keys).zip(&layout.keys) {
            let ty = qir_type(e.ty)?;
            let x = self.b.load(ty, entry, Val::NONE, 1, 8 + f.offset as i32, 0);
            let eq = if ty == Ty::Str16 {
                self.text_eq(x, v, true)
            } else {
                self.b.bin(Op::IcmpEq, x, v)
            };
            let then = self.b.block(&[]);
            self.b.brif(eq, then, &[], advance, &[]);
            self.b.switch_to(then);
        }
        for (j, (f, c)) in layout.payload.iter().zip(&p.columns).enumerate() {
            let ty = qir_type(c.ty)?;
            let v = self.b.load(ty, entry, Val::NONE, 1, 8 + f.offset as i32, 0);
            let ok = self.b.load(Ty::I1, entry, Val::NONE, 1, 8 + f.null() as i32, 0);
            self.loaded.insert(p.first + j, (v, ok));
        }
        Ok((head, e, layout.stride(), advance))
    }

    /// Looks for the group of the key in the key buffer through the words `ht_insert` published
    /// at `at`, the way [`GroupTable`] probes its slots. Returns the block to call `ht_insert` from
    /// when the key is not found or nothing is published yet, and a block with the row as its
    /// parameter that both ways go to. Keys are compared as values and null bytes, which is
    /// enough because a null key's value is zero.
    fn find_group(
        &mut self,
        g: &Grouping,
        values: &[Pair],
        hash: Val,
        at: u32,
        depth: u8,
    ) -> Result<(Block, Option<Block>)> {
        let st = self.b.st();
        let at = at as i32;
        let (slow, found) = (self.b.block(&[]), self.b.block(&[(Ty::Ptr, "row")]));
        let slots = self.b.load(Ty::Ptr, st, Val::NONE, 1, at, 0);
        let word = self.b.conv(Op::Bitcast, slots, Ty::I64);
        let zero = self.b.int(Ty::I64, 0);
        let published = self.b.bin(Op::IcmpNe, word, zero);
        let look = self.b.block(&[]);
        // A table that stopped looking for keys publishes its lanes instead, which a key with no
        // string appends its row to while the page has room and the table says it may.
        let text = g.keys.iter().any(|(k, _)| k.text);
        let blind =
            if text || self.rt.ablate().off(Ablate::APPEND) { slow } else { self.b.block(&[]) };
        self.b.brif(published, look, &[], blind, &[]);
        if blind != slow {
            self.b.switch_to(blind);
            self.append_blind(g, values, hash, at, slow, found);
        }
        self.b.switch_to(look);
        let mask = self.b.load(Ty::I64, st, Val::NONE, 1, at + 8, 0);
        let rows = self.b.load(Ty::Ptr, st, Val::NONE, 1, at + 16, 0);
        let k32 = self.b.int(Ty::I64, 32);
        let tag = self.b.bin(Op::Shl, hash, k32);
        let first = self.b.bin(Op::And, hash, mask);
        let head = self.b.block(&[(Ty::I64, "at")]);
        self.b.br(head, &[first]);
        self.b.switch_to(head);
        // The slots are never more than half full, so the loop ends at an empty one.
        self.b.set_loop(head, depth + 1);
        self.b.set_bounded(head);
        let i = self.b.param(head, 0);
        let slot = self.b.load(Ty::I64, slots, i, 8, 0, 0);
        let empty = self.b.bin(Op::IcmpEq, slot, zero);
        let (test, next) = (self.b.block(&[]), self.b.block(&[]));
        self.b.brif(empty, slow, &[], test, &[]);
        self.b.switch_to(test);
        let high = self.b.konst(Ty::I64, u128::from(!0xffff_ffffu64));
        let hi = self.b.bin(Op::And, slot, high);
        let tagged = self.b.bin(Op::IcmpEq, hi, tag);
        let check = self.b.block(&[]);
        self.b.brif(tagged, check, &[], next, &[]);
        self.b.switch_to(check);
        let low = self.b.int(Ty::I64, 0xffff_ffff);
        let gid = self.b.bin(Op::And, slot, low);
        let row = self.b.load(Ty::Ptr, rows, gid, 8, -8, 0);
        let mut same = self.b.bool(true);
        for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
            let off = (8 + k.offset) as i32;
            let ty = self.b.ty(v);
            let x = self.b.load(ty, row, Val::NONE, 1, off, 0);
            let eq = if ty == Ty::Str16 {
                self.text_eq(x, v, true)
            } else {
                self.b.bin(Op::IcmpEq, x, v)
            };
            let null = self.b.load(Ty::I1, row, Val::NONE, 1, off + k.width as i32, 0);
            // The null byte is set where `ok` is not.
            let flip = self.b.bin(Op::Xor, null, ok);
            same = self.b.bin(Op::And, same, eq);
            same = self.b.bin(Op::And, same, flip);
        }
        let hit = self.b.block(&[]);
        self.b.brif(same, hit, &[], next, &[]);
        self.b.switch_to(hit);
        let n = self.b.load(Ty::I64, st, Val::NONE, 1, at + 24, 0);
        let one = self.b.int(Ty::I64, 1);
        let n = self.b.bin(Op::Add, n, one);
        self.b.store(st, Val::NONE, 1, at + 24, n, 0);
        self.b.br(found, &[row]);
        self.b.switch_to(next);
        let j = self.b.bin(Op::Add, i, one);
        let j = self.b.bin(Op::And, j, mask);
        self.b.br(head, &[j]);
        Ok((slow, Some(found)))
    }

    /// Appends the row of the key in `values` to its lane through the words `ht_insert` published
    /// at `at`, the way [`GroupTable`] does while it makes rows without looking, and goes to
    /// `found` with it. Goes to `slow` when nothing is published, the table may make no more rows
    /// this way or the lane's page is full. The page came zeroed, which is how every accumulator
    /// starts, so only the hash and the key are written.
    fn append_blind(
        &mut self,
        g: &Grouping,
        values: &[Pair],
        hash: Val,
        at: i32,
        slow: Block,
        found: Block,
    ) {
        let st = self.b.st();
        let zero = self.b.int(Ty::I64, 0);
        let tails = self.b.load(Ty::Ptr, st, Val::NONE, 1, at + 32, 0);
        let left = self.b.load(Ty::I64, st, Val::NONE, 1, at + 40, 0);
        let may = self.b.bin(Op::IcmpNe, left, zero);
        let lane = self.b.block(&[]);
        self.b.brif(may, lane, &[], slow, &[]);
        self.b.switch_to(lane);
        let spread = self.b.konst(Ty::I64, u128::from(SPREAD));
        let spread = self.b.bin(Op::Mul, hash, spread);
        let shift = self.b.int(Ty::I64, i128::from(64 - LANE_BITS));
        let index = self.b.bin(Op::Lshr, spread, shift);
        let next = self.b.load(Ty::I64, tails, index, 16, 0, 0);
        let end = self.b.load(Ty::I64, tails, index, 16, 8, 0);
        let full = self.b.bin(Op::IcmpEq, next, end);
        let room = self.b.block(&[]);
        self.b.brif(full, slow, &[], room, &[]);
        self.b.switch_to(room);
        let size = self.b.int(Ty::I64, i128::from(g.row_size));
        let after = self.b.bin(Op::Add, next, size);
        self.b.store(tails, index, 16, 0, after, 0);
        let one = self.b.int(Ty::I64, 1);
        let left = self.b.bin(Op::Sub, left, one);
        self.b.store(st, Val::NONE, 1, at + 40, left, 0);
        let row = self.b.conv(Op::Bitcast, next, Ty::Ptr);
        self.b.store(row, Val::NONE, 1, 0, hash, 0);
        for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
            let null = self.b.un(Op::Not, ok);
            self.b.store(row, Val::NONE, 1, 8 + k.offset as i32, v, 0);
            self.b.store(row, Val::NONE, 1, 8 + k.null() as i32, null, 0);
        }
        self.b.br(found, &[row]);
    }

    fn sink(&mut self, sink: &Sink, out: &Out, skip: Block, depth: u8) -> Result<()> {
        let st = self.b.st();
        match (sink, out) {
            (Sink::Result { exprs, .. }, Out::Result { count, columns, capacity }) => {
                let n = self.b.load(Ty::I64, st, Val::NONE, 1, *count as i32, 0);
                if let Some(at) = capacity {
                    // A probe can make more rows than the morsel has. When the buffers are full,
                    // the driver grows them and runs the morsel again from the start.
                    let cap = self.b.load(Ty::I64, st, Val::NONE, 1, *at as i32, 0);
                    let room = self.b.bin(Op::IcmpUlt, n, cap);
                    let (fits, full) = (self.b.block(&[]), self.b.block(&[]));
                    self.b.set_cold(full);
                    self.b.brif(room, fits, &[], full, &[]);
                    self.b.switch_to(full);
                    let status = self.b.int(Ty::I64, i128::from(NEED_MEMORY));
                    self.b.ret(status);
                    self.b.switch_to(fits);
                }
                for (k, (e, slot)) in exprs.iter().zip(columns).enumerate() {
                    let (v, ok) = self.expr(e)?;
                    let (values, valid) = (self.ptrs[2 * k], self.ptrs[2 * k + 1]);
                    self.b.store(values, n, slot.ty.bytes(), 0, v, 0);
                    self.b.store(valid, n, 1, 0, ok, 0);
                }
                let one = self.b.int(Ty::I64, 1);
                let n1 = self.b.bin(Op::Add, n, one);
                self.b.store(st, Val::NONE, 1, *count as i32, n1, 0);
                Ok(())
            }
            (Sink::Aggregate { groups, aggregates, .. }, Out::Aggregate(g)) => {
                let row = match g.row {
                    Some(at) => self.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, 4),
                    None => {
                        let mut values = Vec::with_capacity(groups.len());
                        for e in groups {
                            let (v, ok) = self.expr(e)?;
                            // A null key's value is zeroed, so that keys the same are the same
                            // bytes for the probe below.
                            let v = if g.probe.is_some() || g.runs.is_some() {
                                let zero = self.b.konst(self.b.ty(v), 0);
                                self.b.select(ok, v, zero)
                            } else {
                                v
                            };
                            values.push((v, ok));
                        }
                        // The key buffer still holds the last row's key, which a key the same
                        // bytes as it finds the same group, so its row is taken again.
                        let done = self.b.block(&[(Ty::Ptr, "row")]);
                        let dense = g.dense.map(|at| self.dense_row(at, done));
                        let keyed = match self.keyed {
                            Some((rows, codes)) if g.runs.is_none() => {
                                Some(self.keyed_row(rows, codes, values[0].1, done))
                            }
                            _ => None,
                        };
                        if let Some(last) = g.last {
                            let seen = self.b.load(Ty::I64, st, Val::NONE, 1, last as i32, 0);
                            let zero = self.b.int(Ty::I64, 0);
                            let mut same = self.b.bin(Op::IcmpNe, seen, zero);
                            for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
                                let at = (SINK + k.offset) as i32;
                                let ty = self.b.ty(v);
                                let old = self.b.load(ty, st, Val::NONE, 1, at, 0);
                                // A long string in the buffer may point at a morsel that is gone,
                                // so only a key the header holds whole is taken as the same here.
                                let eq = if ty == Ty::Str16 {
                                    self.text_eq(old, v, false)
                                } else {
                                    self.b.bin(Op::IcmpEq, old, v)
                                };
                                let null =
                                    self.b.load(Ty::I1, st, Val::NONE, 1, at + k.width as i32, 0);
                                // The null byte is set where `ok` is not.
                                let flip = self.b.bin(Op::Xor, null, ok);
                                same = self.b.bin(Op::And, same, eq);
                                same = self.b.bin(Op::And, same, flip);
                            }
                            let seen = self.b.conv(Op::Bitcast, seen, Ty::Ptr);
                            let new = self.b.block(&[]);
                            self.b.brif(same, done, &[seen], new, &[]);
                            self.b.switch_to(new);
                        }
                        if let Some(at) = g.runs {
                            let row = self.open_run(g, &values, at);
                            self.b.br(done, &[row]);
                            self.b.switch_to(done);
                            self.b.param(done, 0)
                        } else {
                            let mut hash = self.b.int(Ty::I64, 0);
                            for (&(v, ok), (e, (k, _))) in
                                values.iter().zip(groups.iter().zip(&g.keys))
                            {
                                let at = (SINK + k.offset) as i32;
                                self.b.store(st, Val::NONE, 1, at, v, 0);
                                let null = self.b.un(Op::Not, ok);
                                self.b.store(st, Val::NONE, 1, at + k.width as i32, null, 0);
                                hash = self.hash(hash, v, e.ty)?;
                                let ok = self.b.conv(Op::Zext, ok, Ty::I64);
                                hash = self.b.bin(Op::Crc32c, hash, ok);
                            }
                            let key = self.offset(st, SINK);
                            let table = self.handle(g.table);
                            let (slow, found) = match g.probe {
                                Some(at) => self.find_group(g, &values, hash, at, depth)?,
                                None => (self.b.current(), None),
                            };
                            self.b.switch_to(slow);
                            let published = match g.probe {
                                Some(at) => self.offset(st, at),
                                None => self.b.konst(Ty::Ptr, 0),
                            };
                            let row =
                                self.rt(proxy_id("ht_insert"), &[table, key, hash, published]);
                            let row = match found {
                                Some(found) => {
                                    self.b.br(found, &[row]);
                                    self.b.switch_to(found);
                                    self.b.param(found, 0)
                                }
                                None => row,
                            };
                            if let Some(last) = g.last {
                                self.b.store(st, Val::NONE, 1, last as i32, row, 0);
                            }
                            if let Some((entry, used)) = keyed {
                                let (fill, on) = (self.b.block(&[]), self.b.block(&[]));
                                self.b.brif(used, fill, &[], on, &[]);
                                self.b.switch_to(fill);
                                let row = self.b.conv(Op::Bitcast, row, Ty::I64);
                                self.b.store(entry, Val::NONE, 1, 0, row, 0);
                                self.b.br(on, &[]);
                                self.b.switch_to(on);
                            }
                            if let (Some(at), Some((slot, known))) = (g.dense, dense) {
                                // A key with a value not in the domains stores a zero in the entry
                                // past the last, which keeps it zero.
                                let row = self.b.conv(Op::Bitcast, row, Ty::I64);
                                let zero = self.b.int(Ty::I64, 0);
                                let row = self.b.select(known, row, zero);
                                self.b.store(st, slot, 8, at as i32, row, 0);
                            }
                            self.b.br(done, &[row]);
                            self.b.switch_to(done);
                            self.b.param(done, 0)
                        }
                    }
                };
                for (a, acc) in aggregates.iter().zip(&g.accs) {
                    self.update(a, acc, row, g.acc_offset + acc.offset)?;
                }
                Ok(())
            }
            (Sink::Build { keys, payload, marked, .. }, Out::Build(b)) => {
                // A row with a null key can never match, so it stays out of the table, unless an
                // anti join keeps it. Then it goes in with its null byte clear, and the rows are
                // read back by that byte and not by the mark a probe that took it for a zero
                // might set.
                let keep = *marked == Some(JoinType::Anti);
                let mut hash = self.b.int(Ty::I64, 0);
                let mut values = Vec::with_capacity(keys.len() + payload.len());
                for e in keys {
                    let (v, ok) = self.expr(e)?;
                    if !keep {
                        let then = self.b.block(&[]);
                        self.b.brif(ok, then, &[], skip, &[]);
                        self.b.switch_to(then);
                    }
                    hash = self.hash(hash, v, e.ty)?;
                    values.push((v, ok));
                }
                for e in payload {
                    values.push(self.expr(e)?);
                }
                let fields = b.layout.keys.iter().chain(&b.layout.payload);
                for ((v, ok), f) in values.into_iter().zip(fields) {
                    let at = (b.record + f.offset) as i32;
                    self.b.store(st, Val::NONE, 1, at, v, 0);
                    self.b.store(st, Val::NONE, 1, at + f.width as i32, ok, 0);
                }
                let record = self.offset(st, b.record);
                let table = self.handle(b.table);
                self.rt(proxy_id("jt_append"), &[table, record, hash]);
                Ok(())
            }
            (Sink::Mark, Out::Mark) => Ok(()),
            _ => Err(Refusal::new("the sink", "its layout is of the other kind")),
        }
    }

    /// Opens the group of a new run of the key `values`: the key goes in the key buffer, which the
    /// next row compares with, and in the next row of the table's last page, which is zeroed as a
    /// new group's accumulators are. A full page asks the table for another. Returns the row.
    fn open_run(&mut self, g: &Grouping, values: &[Pair], at: u32) -> Val {
        let st = self.b.st();
        let row_size = g.row_size as usize;
        let next = self.b.load(Ty::I64, st, Val::NONE, 1, at as i32, 0);
        let end = self.b.load(Ty::I64, st, Val::NONE, 1, at as i32 + 8, 0);
        let full = self.b.bin(Op::IcmpEq, next, end);
        let (page, fresh) = (self.b.block(&[]), self.b.block(&[(Ty::I64, "next")]));
        self.b.brif(full, page, &[], fresh, &[next]);
        self.b.switch_to(page);
        let table = self.handle(g.table);
        let first = self.rt(proxy_id("ht_page"), &[table]);
        let first = self.b.conv(Op::Bitcast, first, Ty::I64);
        let bytes = self.b.int(Ty::I64, (ROWS_PER_PAGE * row_size) as i128);
        let past = self.b.bin(Op::Add, first, bytes);
        self.b.store(st, Val::NONE, 1, at as i32 + 8, past, 0);
        self.b.br(fresh, &[first]);
        self.b.switch_to(fresh);
        let next = self.b.param(fresh, 0);
        let size = self.b.int(Ty::I64, row_size as i128);
        let after = self.b.bin(Op::Add, next, size);
        self.b.store(st, Val::NONE, 1, at as i32, after, 0);
        let row = self.b.conv(Op::Bitcast, next, Ty::Ptr);
        for (&(v, ok), (k, _)) in values.iter().zip(&g.keys) {
            let key = (SINK + k.offset) as i32;
            let null = self.b.un(Op::Not, ok);
            self.b.store(st, Val::NONE, 1, key, v, 0);
            self.b.store(st, Val::NONE, 1, key + k.width as i32, null, 0);
            self.b.store(row, Val::NONE, 1, 8 + k.offset as i32, v, 0);
            self.b.store(row, Val::NONE, 1, 8 + k.null() as i32, null, 0);
        }
        if let Some(last) = g.last {
            self.b.store(st, Val::NONE, 1, last as i32, row, 0);
        }
        row
    }

    /// Looks the row's key up in the array of [`Body::keyed`] at `rows` by its code, and goes to
    /// `done` with the group row when the code has one. A null key and a morsel with no array
    /// take the way through the hash. Returns the entry's address and whether the array is used,
    /// for that way to fill the entry in.
    fn keyed_row(&mut self, rows: Val, codes: Val, ok: Val, done: Block) -> (Val, Val) {
        let word = self.b.conv(Op::Bitcast, rows, Ty::I64);
        let zero = self.b.int(Ty::I64, 0);
        let held = self.b.bin(Op::IcmpNe, word, zero);
        let used = self.b.bin(Op::And, held, ok);
        let code = self.code(codes);
        let eight = self.b.int(Ty::I64, 8);
        let bytes = self.b.bin(Op::Mul, code, eight);
        let entry = self.b.bin(Op::Add, word, bytes);
        let entry = self.b.conv(Op::Bitcast, entry, Ty::Ptr);
        let (look, miss) = (self.b.block(&[]), self.b.block(&[]));
        self.b.brif(used, look, &[], miss, &[]);
        self.b.switch_to(look);
        let row = self.b.load(Ty::I64, entry, Val::NONE, 1, 0, 0);
        let hit = self.b.bin(Op::IcmpNe, row, zero);
        let row = self.b.conv(Op::Bitcast, row, Ty::Ptr);
        self.b.brif(hit, done, &[row], miss, &[]);
        self.b.switch_to(miss);
        (entry, used)
    }

    /// Looks the row's key up in the array of [`Grouping::dense`] at `at`, and goes to `done` with
    /// the group row when the key has one there. Returns the key's entry and whether every key
    /// value is in its domain, for the way through the hash to fill the entry in.
    fn dense_row(&mut self, at: u32, done: Block) -> (Val, Val) {
        let st = self.b.st();
        let mut slot = self.b.int(Ty::I64, 0);
        let mut known = self.truth();
        let mut stride = 1u32;
        for (values, codes, n) in self.dense.clone() {
            let code = self.code(codes);
            let index = self.b.load(Ty::I16, values, code, 2, 0, 0);
            let index = self.b.conv(Op::Zext, index, Ty::I64);
            let most = self.b.int(Ty::I64, i128::from(n));
            let within = self.b.bin(Op::IcmpUle, index, most);
            known = self.b.bin(Op::And, known, within);
            let step = self.b.int(Ty::I64, i128::from(stride));
            let part = self.b.bin(Op::Mul, index, step);
            slot = self.b.bin(Op::Add, slot, part);
            stride *= n + 1;
        }
        let past = self.b.int(Ty::I64, i128::from(stride));
        let slot = self.b.select(known, slot, past);
        let seen = self.b.load(Ty::I64, st, slot, 8, at as i32, 0);
        let zero = self.b.int(Ty::I64, 0);
        let hit = self.b.bin(Op::IcmpNe, seen, zero);
        let seen = self.b.conv(Op::Bitcast, seen, Ty::Ptr);
        let miss = self.b.block(&[]);
        self.b.brif(hit, done, &[seen], miss, &[]);
        self.b.switch_to(miss);
        (slot, known)
    }

    fn hash(&mut self, hash: Val, v: Val, logical: Type) -> Result<Val> {
        let ty = self.b.ty(v);
        Ok(match ty {
            Ty::Str16 => {
                // A string the header holds whole is hashed as the header's two words, which are
                // the same for the same string because `text::make` zeroes what the string leaves.
                // A longer one never has the same length, so it can hash its bytes.
                let inline = self.b.un(Op::StrInl, v);
                let (short, long) = (self.b.block(&[]), self.b.block(&[]));
                let done = self.b.block(&[(Ty::I64, "hash")]);
                self.b.brif(inline, short, &[], long, &[]);
                self.b.switch_to(short);
                let w0 = self.b.un(Op::StrW0, v);
                let w1 = self.b.un(Op::StrW1, v);
                let h = self.b.bin(Op::Crc32c, hash, w0);
                let h = self.b.bin(Op::Crc32c, h, w1);
                self.b.br(done, &[h]);
                self.b.switch_to(long);
                let h = self.rt(proxy_id("str_hash"), &[v, hash]);
                self.b.br(done, &[h]);
                self.b.switch_to(done);
                self.b.param(done, 0)
            }
            Ty::I128 => {
                let lo = self.b.conv(Op::Trunc, v, Ty::I64);
                let k = self.b.int(Ty::I128, 64);
                let hi = self.b.bin(Op::Lshr, v, k);
                let hi = self.b.conv(Op::Trunc, hi, Ty::I64);
                let h = self.b.bin(Op::Crc32c, hash, lo);
                self.b.bin(Op::Crc32c, h, hi)
            }
            Ty::I64 => self.b.bin(Op::Crc32c, hash, v),
            Ty::I1 | Ty::I8 | Ty::I16 | Ty::I32 => {
                let op = if unsigned(logical) { Op::Zext } else { Op::Sext };
                let x = self.b.conv(op, v, Ty::I64);
                self.b.bin(Op::Crc32c, hash, x)
            }
            _ => return Err(Refusal::new(format!("grouping by {logical}"), "the key has no hash")),
        })
    }

    /// Whether two strings are the same. Headers with the same first word and a string the header
    /// holds whole are the same exactly when their second words are, and so are two long headers
    /// pointing at the same bytes, so the runtime is asked only about two long strings of one
    /// length and prefix in different places. Without `call` that case is taken as not the same.
    fn text_eq(&mut self, a: Val, b: Val, call: bool) -> Val {
        let w0 = self.b.un(Op::StrW0, a);
        let x0 = self.b.un(Op::StrW0, b);
        let w1 = self.b.un(Op::StrW1, a);
        let x1 = self.b.un(Op::StrW1, b);
        let head = self.b.bin(Op::IcmpEq, w0, x0);
        let tail = self.b.bin(Op::IcmpEq, w1, x1);
        let both = self.b.bin(Op::And, head, tail);
        let inline = self.b.un(Op::StrInl, b);
        if !call {
            return self.b.bin(Op::And, both, inline);
        }
        let differ = self.b.un(Op::Not, head);
        let known = self.b.bin(Op::Or, tail, inline);
        let known = self.b.bin(Op::Or, known, differ);
        let (ask, done) = (self.b.block(&[]), self.b.block(&[(Ty::I1, "same")]));
        self.b.brif(known, done, &[both], ask, &[]);
        self.b.switch_to(ask);
        let same = self.rt(proxy_id("str_eq"), &[a, b]);
        self.b.br(done, &[same]);
        self.b.switch_to(done);
        self.b.param(done, 0)
    }

    /// Runs `f` only when `c` holds, and carries on after it either way.
    fn when(&mut self, c: Val, f: impl FnOnce(&mut Gen<'_>) -> Result<()>) -> Result<()> {
        let (yes, after) = (self.b.block(&[]), self.b.block(&[]));
        self.b.brif(c, yes, &[], after, &[]);
        self.b.switch_to(yes);
        f(self)?;
        self.b.br(after, &[]);
        self.b.switch_to(after);
        Ok(())
    }

    fn update(&mut self, a: &Aggregate, acc: &Acc, row: Val, at: u32) -> Result<()> {
        let d = at as i32;
        // The filter clause, and then the argument's validity, decide whether the row counts.
        let mut take = self.truth();
        if let Some(f) = &a.filter {
            let (v, ok) = self.expr(f)?;
            let pass = self.b.bin(Op::And, v, ok);
            take = self.b.bin(Op::And, take, pass);
        }
        let (v, ok) = match a.args.first() {
            Some(x) => self.expr(x)?,
            None => (Val::NONE, self.truth()),
        };
        let take = self.b.bin(Op::And, take, ok);
        match acc.op {
            AccOp::CountStar | AccOp::Count => {
                let n = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
                let add = self.b.conv(Op::Zext, take, Ty::I64);
                let n = self.b.bin(Op::Add, n, add);
                self.b.store(row, Val::NONE, 1, d, n, 0);
            }
            AccOp::SumSplit | AccOp::AvgSplit => {
                let x = self.widen(v, acc.arg, Ty::I64);
                let zero = self.zero(Ty::I64);
                let x = self.b.select(take, x, zero);
                let part = self.b.load(Ty::I64, row, Val::NONE, 1, d + 16, 0);
                let (added, spill) = (self.b.block(&[(Ty::I64, "part")]), self.b.block(&[]));
                self.b.set_cold(spill);
                self.b.checked_edge(Op::SaddOv, part, x, added, spill);
                // The part would overflow, so it goes into the total and starts again at `x`.
                self.b.switch_to(spill);
                let total = self.b.load(Ty::I128, row, Val::NONE, 1, d, 0);
                let wide = self.b.conv(Op::Sext, part, Ty::I128);
                let total = self.b.bin(Op::Add, total, wide);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.b.br(added, &[x]);
                self.b.switch_to(added);
                let part = self.b.param(added, 0);
                self.b.store(row, Val::NONE, 1, d + 16, part, 0);
                self.bump(acc.op == AccOp::AvgSplit, take, row, d + 24);
            }
            AccOp::SumNarrow | AccOp::AvgNarrow => {
                let x = self.widen(v, acc.arg, Ty::I64);
                let zero = self.zero(Ty::I64);
                let x = self.b.select(take, x, zero);
                let total = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
                let total = self.b.bin(Op::Add, total, x);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.bump(acc.op == AccOp::AvgNarrow, take, row, d + 8);
            }
            AccOp::SumFloat => {
                // `float8pl` from the first valid value on, with its overflow check.
                let total = self.b.load(Ty::F64, row, Val::NONE, 1, d, 0);
                let seen = self.b.load(Ty::I1, row, Val::NONE, 1, d + 8, 0);
                let sum = self.b.bin(Op::Fadd, total, v);
                let (s_inf, t_inf, v_inf) = (self.is_inf(sum), self.is_inf(total), self.is_inf(v));
                let t_fin = self.b.un(Op::Not, t_inf);
                let v_fin = self.b.un(Op::Not, v_inf);
                let over = self.b.bin(Op::And, s_inf, t_fin);
                let over = self.b.bin(Op::And, over, v_fin);
                let both = self.b.bin(Op::And, take, seen);
                let over = self.b.bin(Op::And, over, both);
                self.fail_if(over, ErrorKind::Overflow, "value out of range: overflow");
                let next = self.b.select(seen, sum, v);
                let total = self.b.select(take, next, total);
                self.b.store(row, Val::NONE, 1, d, total, 0);
                self.bump(false, take, row, d + 8);
            }
            AccOp::Min | AccOp::Max | AccOp::AnyValue => {
                let ty = self.b.ty(v);
                let w = ty.bytes() as i32;
                let seen = self.b.load(Ty::I1, row, Val::NONE, 1, d + w, 0);
                let first = self.b.un(Op::Not, seen);
                let better = match acc.op {
                    AccOp::AnyValue => self.b.bool(false),
                    op => {
                        let old = self.b.load(ty, row, Val::NONE, 1, d, 0);
                        let (x, y) = if op == AccOp::Min { (v, old) } else { (old, v) };
                        if ty == Ty::Str16 {
                            unreachable!("strings have their own accumulators");
                        } else if ty.is_float() {
                            // `float8smaller` keeps the old value only when it is less, so a
                            // value equal to it, as -0 is to 0, takes its place.
                            let keep = self.b.bin(Op::FcmpLt, y, x);
                            self.b.un(Op::Not, keep)
                        } else if unsigned(acc.arg) {
                            self.b.bin(Op::IcmpUlt, x, y)
                        } else {
                            self.b.bin(Op::IcmpSlt, x, y)
                        }
                    }
                };
                let replace = self.b.bin(Op::Or, first, better);
                let replace = self.b.bin(Op::And, take, replace);
                if ty == Ty::Str16 {
                    self.when(replace, |g| {
                        let none = g.handle(0);
                        let kept = g.rt(proxy_id("str_promote"), &[none, v]);
                        g.b.store(row, Val::NONE, 1, d, kept, 0);
                        let yes = g.truth();
                        g.b.store(row, Val::NONE, 1, d + w, yes, 0);
                        Ok(())
                    })?;
                } else {
                    let old = self.b.load(ty, row, Val::NONE, 1, d, 0);
                    let new = self.b.select(replace, v, old);
                    self.b.store(row, Val::NONE, 1, d, new, 0);
                    let seen = self.b.bin(Op::Or, seen, take);
                    self.b.store(row, Val::NONE, 1, d + w, seen, 0);
                }
            }
            AccOp::MinStr | AccOp::MaxStr => {
                let id =
                    proxy_id(if acc.op == AccOp::MinStr { "agg_min_str" } else { "agg_max_str" });
                self.when(take, |g| {
                    let at = g.offset(row, at);
                    g.b.rtcall(id, &[at, v]);
                    Ok(())
                })?;
            }
            AccOp::Distinct(h, Some(at)) => {
                self.when(take, |g| {
                    let st = g.b.st();
                    let x = g.widen(v, acc.arg, Ty::I64);
                    let cur = g.b.load(Ty::I64, st, Val::NONE, 1, at as i32, 0);
                    let end = g.b.load(Ty::I64, st, Val::NONE, 1, at as i32 + 8, 0);
                    let full = g.b.bin(Op::IcmpEq, cur, end);
                    let (room, append) = (g.b.block(&[]), g.b.block(&[]));
                    g.b.set_cold(room);
                    g.b.brif(full, room, &[], append, &[]);
                    g.b.switch_to(room);
                    let set = g.handle(h);
                    let words = g.offset(st, at);
                    g.b.rtcall(proxy_id("agg_distinct_room"), &[set, words]);
                    g.b.br(append, &[]);
                    g.b.switch_to(append);
                    let cur = g.b.load(Ty::Ptr, st, Val::NONE, 1, at as i32, 0);
                    let gid = g.b.load(Ty::I64, row, Val::NONE, 1, 0, 0);
                    g.b.store(cur, Val::NONE, 1, 0, x, 0);
                    g.b.store(cur, Val::NONE, 1, 8, gid, 0);
                    let next = g.offset(cur, 16);
                    g.b.store(st, Val::NONE, 1, at as i32, next, 0);
                    Ok(())
                })?;
            }
            AccOp::Distinct(h, None) => {
                let ty = self.b.ty(v);
                self.when(take, |g| {
                    let set = g.handle(h);
                    if ty == Ty::Str16 {
                        g.b.rtcall(proxy_id("agg_distinct"), &[set, row, v]);
                    } else {
                        let x = g.widen(v, acc.arg, Ty::I128);
                        g.b.rtcall(proxy_id("agg_distinct_int"), &[set, row, x]);
                    }
                    Ok(())
                })?;
            }
        }
        Ok(())
    }

    /// Adds one to the count at `d` when `take`, or for a sum marks the total seen.
    fn bump(&mut self, count: bool, take: Val, row: Val, d: i32) {
        if count {
            let n = self.b.load(Ty::I64, row, Val::NONE, 1, d, 0);
            let add = self.b.conv(Op::Zext, take, Ty::I64);
            let n = self.b.bin(Op::Add, n, add);
            self.b.store(row, Val::NONE, 1, d, n, 0);
        } else {
            let seen = self.b.load(Ty::I1, row, Val::NONE, 1, d, 0);
            let seen = self.b.bin(Op::Or, seen, take);
            self.b.store(row, Val::NONE, 1, d, seen, 0);
        }
    }

    fn widen(&mut self, v: Val, logical: Type, to: Ty) -> Val {
        if self.b.ty(v) == to {
            return v;
        }
        let op = if unsigned(logical) { Op::Zext } else { Op::Sext };
        self.b.conv(op, v, to)
    }
}

fn is_empty_string(e: &Expr) -> bool {
    matches!(&e.kind, Kind::Constant(Value::Text(s)) if s.is_empty())
        || matches!(&e.kind, Kind::Constant(Value::Bytea(b)) if b.is_empty())
}

fn proxy_id(name: &str) -> u32 {
    proxy(name).unwrap_or_else(|| unreachable!("{name} is in the catalogue"))
}

/// The bytes an accumulator takes in a row.
fn acc_size(acc: &Acc) -> Result<u32> {
    let w = qir_type(acc.arg)?.bytes();
    Ok(match acc.op {
        AccOp::CountStar | AccOp::Count => 8,
        AccOp::SumSplit | AccOp::AvgSplit => 32,
        AccOp::SumNarrow | AccOp::AvgNarrow | AccOp::SumFloat => 16,
        AccOp::Min | AccOp::Max | AccOp::AnyValue => (w + 1).next_multiple_of(8),
        AccOp::MinStr | AccOp::MaxStr => 24,
        AccOp::Distinct(..) => 0,
    })
}

/// Whether a distinct count of `arg` appends its values to the state for the driver, which takes
/// a number that fits in 64 bits.
fn appends(arg: Type) -> Result<bool> {
    let ty = qir_type(arg)?;
    Ok(ty.is_int() && (ty.bytes() < 8 || (ty.bytes() == 8 && !unsigned(arg))))
}

#[cfg(test)]
mod tests;

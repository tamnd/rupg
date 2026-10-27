//! PL/pgSQL, trigger execution, event triggers.
//!
//! This crate first ships in milestone M3 with the functions in PL/pgSQL and the code of `DO`. Triggers and event triggers come later. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.
//!
//! The compiler reads the body of a function into a tree, as `pl_gram.y` and `pl_comp.c` do, and the executor runs the tree, as `pl_exec.c` does. The queries of the function run through [`Spi`], which the session gives, as SPI runs them for PostgreSQL.

#![forbid(unsafe_code)]

mod ast;
mod compile;
mod exec;
mod scan;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{Env, types};
use rupg_catalog::Catalog;
use rupg_common::{Error, Result, SqlState};
use rupg_exec::Plan;
use rupg_func::Session;
use rupg_sql::nodes::{DoStmt, FetchDirection, Node};
use rupg_types::{Value, oid};

use crate::compile::{Arg, Signature, compile};
use crate::exec::{Compiled, Frame};

/// The OID of the language `plpgsql`.
pub const PLPGSQL_LANGUAGE: u32 = 13674;

/// What a function in PL/pgSQL needs of the session, as SPI gives it to PostgreSQL: the catalog, the run of a plan and the subtransactions of the blocks with `EXCEPTION`.
pub trait Spi {
    /// The catalog and the settings that the analysis of a query reads.
    fn env(&self) -> &dyn Env;
    /// The session that a plan runs in.
    fn session(&self) -> &dyn Session;
    /// The catalog that the statement sees. The compiled functions go when it changes.
    fn catalog(&self) -> Arc<Catalog>;
    /// `SPI_execute_plan`: runs the plan with the values `params` of its parameters. It gives the rows of the result, which are the rows of `RETURNING` for a write, and the count of the rows that the query read or wrote.
    ///
    /// # Errors
    ///
    /// The error of the query.
    fn execute(&self, plan: &Plan, params: &[Value]) -> Result<(Vec<Vec<Value>>, u64)>;
    /// `SPI_cursor_find`: true when a portal has the name `name`.
    fn cursor_exists(&self, name: &str) -> bool;
    /// `SPI_cursor_open_with_paramlist`: opens a cursor on the rows of the plan, with the name `name`, or with a new name when it is `None`. `text` is the query of the cursor, and `options` has the bits of `CURSOR_OPT_SCROLL` and `CURSOR_OPT_NO_SCROLL`. It gives the name of the cursor.
    ///
    /// # Errors
    ///
    /// The error of a plan that cannot be a cursor.
    fn open_cursor(
        &self,
        name: Option<&str>,
        plan: &Plan,
        params: &[Value],
        text: &str,
        options: i32,
    ) -> Result<String>;
    /// `SPI_scroll_cursor_fetch`, or `SPI_scroll_cursor_move` when `rows` is false: a fetch or a move of `count` rows from the cursor `name` in the direction `direction`.
    ///
    /// # Errors
    ///
    /// `34000` when there is no such cursor, and the errors of the move.
    fn fetch_cursor(
        &self,
        name: &str,
        direction: FetchDirection,
        count: i64,
        rows: bool,
    ) -> Result<Fetched>;
    /// `SPI_cursor_close`.
    ///
    /// # Errors
    ///
    /// `34000` when there is no such cursor.
    fn close_cursor(&self, name: &str) -> Result<()>;
    /// `CreateNewPortal`: a new name of the form `<unnamed portal n>`. The loop over a query takes one, as its rows come from a portal with no name.
    fn new_portal_name(&self) -> String;
    /// `BeginInternalSubTransaction`: a mark of the writes so far, for [`Spi::rollback_sub`].
    fn begin_sub(&self) -> usize;
    /// `ReleaseCurrentSubTransaction`: the writes after the mark `sub` stay.
    fn release_sub(&self, sub: usize);
    /// `RollbackAndReleaseCurrentSubTransaction`: the writes after the mark `sub` go.
    fn rollback_sub(&self, sub: usize);
    /// `SPI_commit` and `SPI_rollback`, or `SPI_commit_and_chain` and `SPI_rollback_and_chain` when `chain` is true: the transaction ends, and a new transaction starts at once.
    ///
    /// # Errors
    ///
    /// `2D000` in a call that cannot end the transaction and in a subtransaction, `55000` in a loop over a query that writes, and the errors of the deferred triggers at the commit.
    fn end_transaction(&self, commit: bool, chain: bool) -> Result<()>;
    /// `ProcessUtility` of a statement that is not a query, such as `CREATE TABLE` or `SET`, in a call that is not at the top level. It gives the number of the rows that the statement wrote, as `CREATE TABLE AS` gives it.
    ///
    /// # Errors
    ///
    /// The errors of the statement, and `0A000` for a statement that a call cannot run yet.
    fn utility(&self, stmt: &Node, text: &str, notices: &mut Vec<Error>) -> Result<u64>;
    /// The number of the changes of the catalog in the calls that run. A plan made before a change is made again.
    fn changes(&self) -> u64;
    /// `PinPortal`: a loop reads the rows of the portal `name`, or of a query when it is `None`. The end of the transaction keeps the portal, and fails when the query of the loop is not `read_only`.
    fn pin(&self, name: Option<&str>, read_only: bool);
    /// `UnpinPortal`: the loop of the last [`Spi::pin`] ends.
    fn unpin(&self);
    /// A message of `RAISE` below the level `ERROR`, which goes to the client.
    fn notice(&self, notice: Error);
    /// The compiled functions and the calls that run, which the session keeps.
    fn cache(&self) -> &Cache;
}

/// The result of [`Spi::fetch_cursor`].
#[derive(Debug)]
pub struct Fetched {
    /// The name, the type and the typmod of each column of the cursor.
    pub columns: Vec<(String, u32, i32)>,
    /// The rows of a fetch.
    pub rows: Vec<Vec<Value>>,
    /// The number of rows that the fetch or the move went over.
    pub count: u64,
}

/// The OID of a function, the actual types of its arguments and its actual result type.
type FunctionKey = (u32, Vec<u32>, u32);

/// The compiled functions of a session, as `plpgsql_HashTable` keeps them, and the stack of the calls that run.
#[derive(Debug, Default)]
pub struct Cache {
    /// The catalog that the functions were compiled with.
    catalog: RefCell<Option<Arc<Catalog>>>,
    /// The functions by their OID, the actual types of the arguments and the actual result type, with the row of `pg_proc` that each was compiled from.
    functions: RefCell<BTreeMap<FunctionKey, (rupg_catalog::Function, Rc<Compiled>)>>,
    /// The calls that run, the innermost last.
    pub(crate) frames: RefCell<Vec<Frame>>,
}

impl Cache {
    /// The compiled functions go when the catalog is not the catalog that they were compiled with.
    fn check_catalog(&self, catalog: Arc<Catalog>) {
        let mut current = self.catalog.borrow_mut();
        if !current.as_ref().is_some_and(|c| Arc::ptr_eq(c, &catalog)) {
            self.functions.borrow_mut().clear();
            *current = Some(catalog);
        }
    }
}

/// `plpgsql_call_handler`: a call of the function `func` with the values `args` of the actual types `types`, for the actual result type `ret`. It gives the value of the function, or a value for each row of a function that returns a set.
///
/// # Errors
///
/// The errors of the compile and of the run of the function.
pub fn call(
    spi: &dyn Spi,
    func: u32,
    types: &[u32],
    ret: u32,
    args: &[Value],
) -> Result<Vec<Value>> {
    let cache = spi.cache();
    cache.check_catalog(spi.catalog());
    let key = (func, types.to_vec(), ret);
    let found = cache.functions.borrow().get(&key).map(|(_, c)| Rc::clone(c));
    let compiled = match found {
        Some(compiled) => compiled,
        None => {
            let catalog = spi.catalog();
            let Some(function) = catalog.function(func) else {
                return Err(Error::internal(format!("cache lookup failed for function {func}")));
            };
            let sig = signature(function, types, ret);
            let tree = compile(&function.src, spi.env(), &sig, false)?;
            let text = rupg_func::to_text(oid::REGPROCEDURE, &Value::Oid(func), spi.session())?;
            let compiled = Rc::new(Compiled::new(tree, text, func));
            cache.functions.borrow_mut().insert(key, (function.clone(), Rc::clone(&compiled)));
            compiled
        }
    };
    exec::run(spi, &compiled, types, ret, args)
}

/// The event of a call of a trigger function, as `TriggerData` gives it.
#[derive(Debug)]
pub struct TriggerCall<'a> {
    /// The trigger.
    pub trigger: &'a rupg_catalog::Trigger,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub when: &'a str,
    /// `ROW` or `STATEMENT`.
    pub level: &'a str,
    /// `INSERT`, `UPDATE`, `DELETE` or `TRUNCATE`.
    pub op: &'a str,
    /// The name of the table.
    pub relname: &'a str,
    /// The name of the schema of the table.
    pub schema: &'a str,
    /// The columns of the table with their types and typmods, with the type 0 for a dropped column.
    pub columns: Vec<(String, u32, i32)>,
    /// The new row of an `INSERT` or of an `UPDATE` for a trigger `FOR EACH ROW`, with a value for each column.
    pub new: Option<Vec<Value>>,
    /// The old row of an `UPDATE` or of a `DELETE` for a trigger `FOR EACH ROW`.
    pub old: Option<Vec<Value>>,
}

/// `plpgsql_exec_trigger`: a call of the trigger function of `call.trigger` for the event `call`. It gives the row that the function returns, with a value for each column of the table, or `None` for a null row and for a trigger `FOR EACH STATEMENT`.
///
/// # Errors
///
/// The errors of the compile and of the run of the function.
pub fn trigger(spi: &dyn Spi, call: &TriggerCall<'_>) -> Result<Option<Vec<Value>>> {
    let cache = spi.cache();
    cache.check_catalog(spi.catalog());
    let func = call.trigger.function;
    let key = (func, vec![call.trigger.relation], oid::TRIGGER);
    let found = cache.functions.borrow().get(&key).map(|(_, c)| Rc::clone(c));
    let compiled = match found {
        Some(compiled) => compiled,
        None => {
            let catalog = spi.catalog();
            let Some(function) = catalog.function(func) else {
                return Err(Error::internal(format!("cache lookup failed for function {func}")));
            };
            let mut sig = signature(function, &[], 0);
            sig.trigger = true;
            let tree = compile(&function.src, spi.env(), &sig, false)?;
            let text = rupg_func::to_text(oid::REGPROCEDURE, &Value::Oid(func), spi.session())?;
            let compiled = Rc::new(Compiled::new(tree, text, func));
            cache.functions.borrow_mut().insert(key, (function.clone(), Rc::clone(&compiled)));
            compiled
        }
    };
    exec::run_trigger(spi, &compiled, call)
}

/// The options of `DO`, as `ExecuteDoStmt` reads them: the code, in a language that can run it.
///
/// # Errors
///
/// `42601` for an option that comes twice and for no code, `42704` for a language that does not exist, and `0A000` for a language with no inline handler.
pub fn do_code(stmt: &DoStmt) -> Result<String> {
    let mut src = None;
    let mut language = None;
    for def in stmt.args.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let slot = match def.defname.as_deref() {
            Some("as") => &mut src,
            Some("language") => &mut language,
            _ => continue,
        };
        if slot.is_some() {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options")
                .with_position(usize::try_from(def.location).ok()));
        }
        *slot = Some(match &def.arg {
            Some(Node::String(s)) => s.to_string(),
            _ => String::new(),
        });
    }
    let Some(src) = src else {
        return Err(Error::new(SqlState::SYNTAX_ERROR, "no inline code specified"));
    };
    let language = language.unwrap_or_else(|| "plpgsql".to_owned());
    match language.as_str() {
        "plpgsql" => Ok(src),
        "internal" | "c" | "sql" => Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("language \"{language}\" does not support inline code execution"),
        )),
        _ => Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("language \"{language}\" does not exist"),
        )),
    }
}

/// `plpgsql_inline_handler`: runs the code of `DO`.
///
/// # Errors
///
/// The errors of the compile and of the run of the code.
pub fn inline(spi: &dyn Spi, src: &str) -> Result<()> {
    let sig = Signature {
        name: "inline_code_block".to_owned(),
        args: Vec::new(),
        rettype: oid::VOID,
        retset: false,
        trigger: false,
        procedure: false,
    };
    let tree = compile(src, spi.env(), &sig, true)?;
    let compiled = Compiled::new(tree, "inline_code_block", 0);
    exec::run(spi, &compiled, &[], oid::VOID, &[]).map(drop)
}

/// `plpgsql_validator`: the compile of the body of a new function, with the check of the syntax of its queries. A polymorphic type compiles as a type of its kind, as `plpgsql_resolve_polymorphic_argtypes` does for the validator. A trigger function compiles as a trigger. An event trigger function is not checked yet.
///
/// # Errors
///
/// The syntax errors of the body.
pub fn validate(env: &dyn Env, function: &rupg_catalog::Function) -> Result<()> {
    if function.lang != PLPGSQL_LANGUAGE || function.rettype == oid::EVENT_TRIGGER {
        return Ok(());
    }
    let dummy = |ty: u32| match ty {
        oid::ANYELEMENT
        | oid::ANYNONARRAY
        | oid::ANYENUM
        | oid::ANYCOMPATIBLE
        | oid::ANYCOMPATIBLENONARRAY => oid::INT4,
        oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY => oid::INT4_ARRAY,
        oid::ANYRANGE | oid::ANYCOMPATIBLERANGE => oid::INT4RANGE,
        oid::ANYMULTIRANGE | oid::ANYCOMPATIBLEMULTIRANGE => oid::INT4MULTIRANGE,
        ty => ty,
    };
    let mut sig = signature(function, &[], 0);
    for arg in &mut sig.args {
        arg.ty = dummy(arg.ty);
    }
    sig.rettype = dummy(function.rettype);
    sig.trigger = function.rettype == oid::TRIGGER;
    sig.name.clone_from(&function.name);
    compile(&function.src, env, &sig, true).map(drop)
}

/// True for a polymorphic type whose actual type is the element type of the call.
fn element_like(ty: u32) -> bool {
    matches!(
        ty,
        oid::ANYELEMENT
            | oid::ANYNONARRAY
            | oid::ANYENUM
            | oid::ANYCOMPATIBLE
            | oid::ANYCOMPATIBLENONARRAY
    )
}

/// True for a polymorphic type whose actual type is the array type of the call.
fn array_like(ty: u32) -> bool {
    matches!(ty, oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY)
}

/// `compute_function_hashkey` and `plpgsql_resolve_polymorphic_argtypes`: the arguments and the result type of the function, with the actual types `types` of the input arguments and the actual result type `ret` for the polymorphic types.
fn signature(function: &rupg_catalog::Function, types: &[u32], ret: u32) -> Signature {
    let all =
        if function.allargtypes.is_empty() { &function.argtypes } else { &function.allargtypes };
    let mode = |i: usize| function.argmodes.get(i).copied().unwrap_or(b'i');
    let input = |m: u8| matches!(m, b'i' | b'b' | b'v');
    // The actual type of each input argument, by its place in all the arguments.
    let mut actual = BTreeMap::new();
    let mut next = types.iter();
    for i in 0..all.len() {
        if input(mode(i))
            && let Some(&ty) = next.next()
        {
            actual.insert(i, ty);
        }
    }
    let elem = all.iter().enumerate().find_map(|(i, &ty)| {
        let found = *actual.get(&i)?;
        if element_like(ty) {
            Some(found)
        } else if array_like(ty) {
            Some(types::element(found)).filter(|&e| e != 0)
        } else {
            None
        }
    });
    let outs = (0..all.len()).filter(|&i| matches!(mode(i), b'o' | b'b' | b't')).count();
    let args = all
        .iter()
        .enumerate()
        .map(|(i, &ty)| {
            let ty = if !types::is_polymorphic(ty) {
                ty
            } else if let Some(&found) = actual.get(&i) {
                found
            } else if outs == 1 && ret != 0 {
                ret
            } else if element_like(ty) && elem.is_some() {
                elem.unwrap_or(ty)
            } else if array_like(ty) && elem.is_some() {
                elem.map_or(ty, types::array_of)
            } else {
                all.iter()
                    .enumerate()
                    .find(|&(j, &other)| other == ty && actual.contains_key(&j))
                    .and_then(|(j, _)| actual.get(&j).copied())
                    .unwrap_or(if ret != 0 { ret } else { ty })
            };
            Arg { name: function.argnames.get(i).cloned().unwrap_or_default(), ty, mode: mode(i) }
        })
        .collect();
    let rettype =
        if types::is_polymorphic(function.rettype) && ret != 0 { ret } else { function.rettype };
    Signature {
        name: function.name.clone(),
        args,
        rettype,
        retset: function.retset,
        trigger: false,
        procedure: function.kind == b'p',
    }
}

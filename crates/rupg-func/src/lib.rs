//! The built-in functions: the input and output functions of the types, and the kernels of the functions, the operators and the casts of `pg_proc`.
//!
//! A kernel is found by the `prosrc` of the function, as the `fmgr` of PostgreSQL finds the C function of an internal function. Many functions of the same kind, such as the comparisons and the arithmetic of the number types, share one kernel that reads the types of the call. A function that has no kernel here is an error `0A000` when the executor compiles the query, so a query never gives a wrong answer for a function that the engine does not have.

#![forbid(unsafe_code)]

mod acl;
mod agg;
mod array;
mod cast;
mod catinfo;
mod compare;
mod deparse;
mod io;
mod math;
mod misc;
mod reg;
mod regex;
mod srf;
mod text;

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::OnceLock;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{ByteaOutput, DateFormat, IntervalStyle, TimeZone, TypeError, Value};

pub use cast::local_time;
pub use compare::{compare, equal};
pub use io::{base_type, input, input_supported, output, output_supported, receive, send, to_text};
pub use srf::{SetKernel, set_kernel};

/// The OID of `internal`, the language of the functions in C.
const INTERNAL_LANGUAGE: u32 = 12;

/// The state of the session that the functions read.
pub trait Session {
    /// The `DateStyle` setting.
    fn date_format(&self) -> DateFormat;
    /// The `IntervalStyle` setting.
    fn interval_style(&self) -> IntervalStyle;
    /// The `TimeZone` setting.
    ///
    /// # Errors
    ///
    /// `0A000` for a zone whose rules the engine does not have yet.
    fn zone(&self) -> Result<Rc<dyn TimeZone>>;
    /// The `extra_float_digits` setting.
    fn extra_float_digits(&self) -> i32;
    /// The `bytea_output` setting.
    fn bytea_output(&self) -> ByteaOutput;
    /// The `array_nulls` setting.
    fn array_nulls(&self) -> bool;
    /// The start of the transaction in microseconds since 2000-01-01 UTC.
    fn transaction_start(&self) -> i64;
    /// The start of the statement in microseconds since 2000-01-01 UTC.
    fn statement_start(&self) -> i64;
    /// The time now in microseconds since 2000-01-01 UTC.
    fn clock(&self) -> i64;
    /// The current user.
    fn user(&self) -> &str;
    /// The session user.
    fn session_user(&self) -> &str;
    /// The current database.
    fn database(&self) -> &str;
    /// The schemas of `search_path` that exist, in order, without the implicit schemas.
    fn schemas(&self) -> Vec<String>;
    /// The process ID of the session.
    fn backend_pid(&self) -> i32;
    /// The text of `version()`.
    fn version(&self) -> String;
    /// The value of a setting as `SHOW` gives it, or `None` for a name that is not a setting.
    fn setting(&self, name: &str) -> Option<String>;
    /// `set_config_option` for `set_config`: sets a parameter for the session, or for the transaction when `local` is true, and gives the new value as `SHOW` gives it. `value` is `None` for a reset.
    ///
    /// # Errors
    ///
    /// The errors of `SET` for the name and the value.
    fn set_setting(&self, name: &str, value: Option<&str>, local: bool) -> Result<String>;
    /// The rows of `pg_show_all_settings`, one for each parameter that `SHOW ALL` lists, in the 17 columns of the function.
    fn all_settings(&self) -> Vec<Vec<Value>> {
        Vec::new()
    }
    /// The catalog of the user objects that the statement sees, or `None` when the session has no catalog.
    fn catalog(&self) -> Option<&rupg_catalog::Catalog> {
        None
    }
}

/// A call of a function.
pub struct Call<'a> {
    pub session: &'a dyn Session,
    /// The types of the arguments as the query gives them.
    pub args: &'a [u32],
    /// The type of the result.
    pub ret: u32,
    /// True when the last argument is the array of a `VARIADIC` call.
    pub variadic: bool,
}

impl std::fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("args", &self.args)
            .field("ret", &self.ret)
            .field("variadic", &self.variadic)
            .finish_non_exhaustive()
    }
}

/// The code of a function. It gets the values of the arguments. A strict function does not get a null value, because the executor gives null without the call.
pub type Kernel = fn(&Call<'_>, &[Value]) -> Result<Value>;

/// The error of a type function as the session sends it.
pub fn type_error(error: TypeError) -> Error {
    let mut out = Error::new(error.sqlstate, error.message);
    if let Some(detail) = error.detail {
        out = out.with_detail(detail);
    }
    if let Some(hint) = error.hint {
        out = out.with_hint(hint);
    }
    out
}

/// The error of a feature that the engine does not have yet.
pub(crate) fn not_yet(what: impl std::fmt::Display) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// The error of a value of a different kind than the type of the call gives, which is a bug of the engine.
pub(crate) fn bad_value() -> Error {
    Error::internal("a function got a value of the wrong kind")
}

/// The kernel of the function with this `pg_proc` OID, or `None` if the engine does not have it.
pub fn kernel(func: u32) -> Option<Kernel> {
    let proc = builtin::proc_by_oid(func)?;
    if proc.lang != INTERNAL_LANGUAGE {
        return misc::sql_function(func);
    }
    if let Some(kernel) = by_src(proc.src).or_else(|| compare::by_src(proc.src, proc)) {
        return Some(kernel);
    }
    let operator = operator_of(func)?;
    let left = proc.argtypes.first().copied().unwrap_or(0);
    let right = proc.argtypes.get(1).copied().unwrap_or(0);
    if proc.argtypes.len() == 2 {
        compare::by_operator(operator, left, right)
            .or_else(|| math::by_operator(operator, left, right, proc.rettype))
    } else {
        math::by_prefix_operator(operator, left, proc.rettype)
    }
}

/// The kernel of an internal function by its `prosrc`.
fn by_src(src: &str) -> Option<Kernel> {
    cast::by_src(src)
        .or_else(|| agg::by_src(src))
        .or_else(|| math::by_src(src))
        .or_else(|| text::by_src(src))
        .or_else(|| misc::by_src(src))
        .or_else(|| reg::by_src(src))
        .or_else(|| catinfo::by_src(src))
        .or_else(|| acl::by_src(src))
        .or_else(|| deparse::by_src(src))
        .or_else(|| array::by_src(src))
        .or_else(|| regex::by_src(src))
}

/// The name of the operator that the function implements, if it implements one.
fn operator_of(func: u32) -> Option<&'static str> {
    static OPERATORS: OnceLock<BTreeMap<u32, &'static str>> = OnceLock::new();
    let map = OPERATORS
        .get_or_init(|| builtin::operators().iter().map(|op| (op.code, op.name)).collect());
    map.get(&func).copied()
}

#[cfg(test)]
mod tests;

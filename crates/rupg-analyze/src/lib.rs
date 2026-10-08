//! Name resolution, type resolution, the query tree, the rewriter for views and rules.
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.
//!
//! [`analyze`] takes a raw statement of `rupg-sql` and gives a [`Query`] whose expressions have their types, as `parse_analyze` of PostgreSQL does. The rules of type resolution come from `parse_coerce.c`, `parse_func.c` and `parse_oper.c` of PostgreSQL 19, so a query gets the same functions, the same casts and the same errors.

#![forbid(unsafe_code)]

mod coerce;
mod colname;
mod expr;
mod from;
mod poly;
mod resolve;
mod select;
mod transform;
mod typename;
pub mod types;

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;
use rupg_types::{Value, oid};

use crate::coerce::AtOpt;

pub use coerce::{Context, Path, can_coerce, find_path};
pub use colname::figure_colname;
pub use expr::{BoolOp, BoolTest, Case, Expr, ExprKind, Func, FuncForm, SqlValue, Var};
pub use from::{Column, FromItem, Join, JoinKind, Relation, TABLE_OID_ATTNUM};
pub use select::{Query, Target};

/// The OID of the schema `pg_catalog`.
pub const PG_CATALOG_NAMESPACE: u32 = 11;

/// The facts of the session that the analyzer reads.
pub trait Env {
    /// The OIDs of the schemas of `search_path` that exist, in order.
    fn search_path(&self) -> Vec<u32>;
    /// The OID of the schema with this name.
    fn namespace(&self, name: &str) -> Option<u32>;
    /// The name of the current database.
    fn database(&self) -> String;
    /// The input function of the type: the value of a string literal. The settings of the session, such as `DateStyle`, apply.
    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value>;
}

/// The parameters of a statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Params {
    /// The type OID of each parameter, from `$1`. 0 means that the client gave no type.
    pub types: Vec<u32>,
    /// True for a statement of the extended protocol, which can use parameters that the client did not declare and gets the types from the query. A simple query has no parameters.
    pub variable: bool,
}

/// The state of the analysis of one statement.
pub(crate) struct Analyzer<'a> {
    env: &'a dyn Env,
    /// The schemas that an unqualified name searches, with `pg_catalog` first when `search_path` does not name it.
    path: Vec<u32>,
    params: Vec<u32>,
    variable: bool,
    /// The warnings and notices of the analysis, such as a precision that is too large.
    notices: Vec<Error>,
    /// The depth of the expression that the analyzer reads now.
    depth: usize,
    /// The relations and the names of `FROM`.
    scope: from::Scope,
}

impl<'a> Analyzer<'a> {
    fn new(env: &'a dyn Env, params: &Params) -> Analyzer<'a> {
        let mut path = env.search_path();
        if !path.contains(&PG_CATALOG_NAMESPACE) {
            path.insert(0, PG_CATALOG_NAMESPACE);
        }
        Analyzer {
            env,
            path,
            params: params.types.clone(),
            variable: params.variable,
            notices: Vec::new(),
            depth: 0,
            scope: from::Scope::default(),
        }
    }

    /// `variable_coerce_param_hook`: a parameter of unknown type that the query casts to a type gets that type. `Ok(None)` means that the parameters are fixed, so the cast is a normal cast.
    pub(crate) fn param_type(
        &mut self,
        n: usize,
        target: u32,
        at: Option<usize>,
    ) -> Result<Option<u32>> {
        if !self.variable || target == oid::UNKNOWN {
            return Ok(None);
        }
        let Some(slot) = self.params.get_mut(n - 1) else { return Ok(None) };
        if *slot == 0 || *slot == oid::UNKNOWN {
            *slot = target;
        } else if *slot != target {
            return Err(Error::new(
                SqlState::AMBIGUOUS_PARAMETER,
                format!("inconsistent types deduced for parameter ${n}"),
            )
            .with_detail(format!("{} versus {}", types::name(*slot), types::name(target)))
            .at_opt(at));
        }
        Ok(Some(target))
    }
}

/// `parse_analyze`: the query tree of a raw statement, which is the `stmt` of a `RawStmt`. For a statement of the extended protocol, the [`Query`] has the type of each parameter.
pub fn analyze(stmt: &Node, env: &dyn Env, params: &Params) -> Result<Query> {
    let mut analyzer = Analyzer::new(env, params);
    let mut query = match stmt {
        Node::SelectStmt(select) => analyzer.select(select)?,
        _ => {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "this statement is not supported yet",
            ));
        }
    };
    if analyzer.variable {
        for (i, ty) in analyzer.params.iter().enumerate() {
            if *ty == 0 || *ty == oid::UNKNOWN {
                return Err(Error::new(
                    SqlState::INDETERMINATE_DATATYPE,
                    format!("could not determine data type of parameter ${}", i + 1),
                ));
            }
        }
    }
    query.params = analyzer.params;
    query.notices = analyzer.notices;
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session with the default `search_path` and the input functions of a few types.
    struct TestEnv;

    impl Env for TestEnv {
        fn search_path(&self) -> Vec<u32> {
            vec![2200]
        }

        fn namespace(&self, name: &str) -> Option<u32> {
            match name {
                "pg_catalog" => Some(PG_CATALOG_NAMESPACE),
                "public" => Some(2200),
                _ => None,
            }
        }

        fn database(&self) -> String {
            "postgres".into()
        }

        fn input(&self, ty: u32, text: &str, _typmod: i32) -> Result<Value> {
            Ok(match ty {
                oid::BOOL => Value::Bool(rupg_types::bool_in(text)?),
                oid::INT4 => Value::Int4(rupg_types::int4_in(text)?),
                oid::INT8 => Value::Int8(rupg_types::int8_in(text)?),
                oid::FLOAT8 => Value::Float8(rupg_types::float8_in(text)?),
                oid::NUMERIC => Value::Numeric(rupg_types::numeric_in(text, -1)?),
                _ => Value::text(text),
            })
        }
    }

    fn run(sql: &str, params: &Params) -> Result<Query> {
        let (stmts, _) = rupg_sql::parse(sql)?;
        let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement in {sql}") };
        analyze(raw.stmt.as_ref().expect("a statement"), &TestEnv, params)
    }

    /// The name and the type of each column.
    fn columns(sql: &str) -> Vec<(String, u32)> {
        let query =
            run(sql, &Params::default()).unwrap_or_else(|e| panic!("{sql}: {}", e.message()));
        query.targets.into_iter().map(|t| (t.name, t.expr.ty)).collect()
    }

    /// The SQLSTATE, the message and the byte offset of the error.
    fn error(sql: &str) -> (String, String, Option<usize>) {
        let e = run(sql, &Params::default()).expect_err(sql);
        (e.state().as_str().to_string(), e.message().to_string(), e.position())
    }

    /// Runs the cases in a thread with 8 MiB of stack. In a debug build the action functions of the parser have large frames, and a statement needs more than the 2 MiB of a test thread.
    // The thread is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn big_stack(cases: fn()) {
        std::thread::Builder::new().stack_size(8 << 20).spawn(cases).unwrap().join().unwrap();
    }

    #[test]
    fn types_of_columns() {
        big_stack(|| {
            let one = |sql: &str| columns(sql).remove(0).1;
            assert_eq!(
                columns("SELECT 1, 'a' AS b"),
                [("?column?".into(), oid::INT4), ("b".into(), oid::TEXT)]
            );
            assert_eq!(one("SELECT 1 + 2.5"), oid::NUMERIC);
            assert_eq!(one("SELECT 'a' || 1"), oid::TEXT);
            assert_eq!(one("SELECT 2147483648"), oid::INT8);
            assert_eq!(one("SELECT 9223372036854775808"), oid::NUMERIC);
            assert_eq!(one("SELECT CASE WHEN true THEN 1 ELSE 2.0 END"), oid::NUMERIC);
            assert_eq!(one("SELECT GREATEST(1, 2.5)"), oid::NUMERIC);
            assert_eq!(one("SELECT current_user"), oid::NAME);
            assert_eq!(one("SELECT now()"), oid::TIMESTAMPTZ);
            assert_eq!(one("SELECT ARRAY[1, 2]"), oid::INT4_ARRAY);
            assert_eq!(one("SELECT '{1}'::int[]"), oid::INT4_ARRAY);
            assert_eq!(one("SELECT 1 IN (1, 2) AND 1 BETWEEN 0 AND 2"), oid::BOOL);
            assert_eq!(one("SELECT NULLIF(1, 2)"), oid::INT4);
            assert_eq!(one("SELECT COALESCE(NULL, 1)"), oid::INT4);
            assert_eq!(one("SELECT abs(-1)"), oid::INT4);
            assert_eq!(one("SELECT length('abc')"), oid::INT4);
            let query = run("SELECT 'abc'::varchar(2)", &Params::default()).unwrap();
            assert_eq!((query.targets[0].expr.ty, query.targets[0].expr.typmod), (oid::VARCHAR, 6));
            let query = run("SELECT localtimestamp(9)", &Params::default()).unwrap();
            assert_eq!(
                query.notices[0].message(),
                "TIMESTAMP(9) precision reduced to maximum allowed, 6"
            );
        });
    }

    #[test]
    fn errors() {
        big_stack(|| {
            let e = |state: &str, message: &str, at: usize| {
                (state.to_string(), message.to_string(), Some(at))
            };
            assert_eq!(error("SELECT x"), e("42703", "column \"x\" does not exist", 7));
            assert_eq!(
                error("SELECT t.x"),
                e("42P01", "missing FROM-clause entry for table \"t\"", 7)
            );
            assert_eq!(
                error("SELECT *"),
                e("42601", "SELECT * with no tables specified is not valid", 7)
            );
            assert_eq!(error("SELECT $1"), e("42P02", "there is no parameter $1", 7));
            assert_eq!(
                error("SELECT 1 + 'a'"),
                e("22P02", "invalid input syntax for type integer: \"a\"", 11)
            );
            assert_eq!(
                error("SELECT 1 IN (1, 'a')"),
                e("22P02", "invalid input syntax for type integer: \"a\"", 16)
            );
            assert_eq!(
                error("SELECT foo(1)"),
                e("42883", "function foo(integer) does not exist", 7)
            );
            assert_eq!(
                error("SELECT lower(1)"),
                e("42883", "function lower(integer) does not exist", 7)
            );
            assert_eq!(
                error("SELECT 1 + true"),
                e("42883", "operator does not exist: integer + boolean", 9)
            );
            assert_eq!(
                error("SELECT ARRAY[]"),
                e("42P18", "cannot determine type of empty array", 7)
            );
            assert_eq!(
                error("SELECT 'a'::varchar(0)"),
                e("22023", "length for type varchar must be at least 1", 12)
            );
            assert_eq!(
                error("SELECT CASE WHEN 1 THEN 1 END"),
                e("42804", "argument of CASE/WHEN must be type boolean, not type integer", 17)
            );
            assert_eq!(
                error("SELECT 'a' COLLATE \"xx\""),
                e("42704", "collation \"xx\" for encoding \"UTF8\" does not exist", 11)
            );
            assert_eq!(
                error("SELECT 1 COLLATE \"C\""),
                e("42804", "collations are not supported by type integer", 9)
            );
            assert_eq!(
                error("SELECT 1 = ANY (1)"),
                e("42809", "op ANY/ALL (array) requires array on right side", 9)
            );
            assert_eq!(
                error("SELECT NULLIF(1, 'a'::text)"),
                e("42883", "operator does not exist: integer = text", 7)
            );
            assert_eq!(
                error("SELECT 1 IS DISTINCT FROM 'x'::text"),
                e("42883", "operator does not exist: integer = text", 9)
            );
            assert_eq!(
                error("SELECT ARRAY[1, 'a'::text]"),
                e("42804", "ARRAY types integer and text cannot be matched", 16)
            );
            assert_eq!(
                error("SELECT COALESCE(1, 'a'::text)"),
                e("42804", "COALESCE types integer and text cannot be matched", 19)
            );
        });
    }

    #[test]
    fn parameters() {
        big_stack(|| {
            let variable = Params { types: Vec::new(), variable: true };
            let query = run("SELECT $1 + 1, $2", &variable).unwrap();
            assert_eq!(query.params, [oid::INT4, oid::TEXT]);
            let e = run("SELECT $1 IS NULL", &variable).unwrap_err();
            assert_eq!(e.message(), "could not determine data type of parameter $1");
            let declared = Params { types: vec![oid::INT8], variable: true };
            assert_eq!(run("SELECT $1 + 1", &declared).unwrap().targets[0].expr.ty, oid::INT8);
        });
    }

    /// The SQLSTATE, the message, the detail and the hint of the error.
    fn full_error(sql: &str) -> [String; 4] {
        let e = run(sql, &Params::default()).expect_err(sql);
        [
            e.state().as_str().to_string(),
            e.message().to_string(),
            e.detail().unwrap_or_default().to_string(),
            e.hint().unwrap_or_default().to_string(),
        ]
    }

    #[test]
    fn from_clause() {
        big_stack(|| {
            assert_eq!(
                columns("SELECT * FROM pg_namespace"),
                [
                    ("oid".into(), oid::OID),
                    ("nspname".into(), oid::NAME),
                    ("nspowner".into(), oid::OID),
                    ("nspacl".into(), oid::ACLITEM_ARRAY)
                ]
            );
            assert_eq!(
                columns("SELECT a.x, n FROM pg_namespace a(x, n)"),
                [("x".into(), oid::OID), ("n".into(), oid::NAME)]
            );
            let names = |sql: &str| -> Vec<String> {
                columns(sql).into_iter().map(|(name, _)| name).collect()
            };
            assert_eq!(
                names("SELECT * FROM pg_namespace a JOIN pg_namespace b USING (oid, nspname)"),
                ["oid", "nspname", "nspowner", "nspacl", "nspowner", "nspacl"]
            );
            assert_eq!(names("SELECT u.* FROM pg_am a JOIN pg_am b USING (oid) AS u"), ["oid"]);
            assert_eq!(
                names("SELECT a.tableoid, b.* FROM pg_am a NATURAL JOIN pg_am b"),
                ["tableoid", "oid", "amname", "amhandler", "amtype"]
            );
            let query = run("SELECT oid, oid::int + 0 FROM pg_type", &Params::default()).unwrap();
            assert_eq!(query.targets[0].origin, Some((1247, 1)));
            assert_eq!(query.targets[1].origin, None);
            let query =
                run("SELECT oid FROM pg_am a FULL JOIN pg_am b USING (oid)", &Params::default())
                    .unwrap();
            assert_eq!(query.targets[0].origin, None);
            assert!(matches!(query.targets[0].expr.kind, ExprKind::Coalesce(_)));
        });
    }

    #[test]
    fn from_errors() {
        big_stack(|| {
            let e = |state: &str, message: &str, at: usize| {
                (state.to_string(), message.to_string(), Some(at))
            };
            assert_eq!(
                error("SELECT 1 FROM nosuch"),
                e("42P01", "relation \"nosuch\" does not exist", 14)
            );
            assert_eq!(
                error("SELECT 1 FROM x.pg_class"),
                e("42P01", "relation \"x.pg_class\" does not exist", 14)
            );
            assert_eq!(
                error("SELECT 1 FROM a.b.pg_class"),
                e("0A000", "cross-database references are not implemented: \"a.b.pg_class\"", 14)
            );
            assert_eq!(
                error("SELECT oid FROM pg_am a, pg_am b"),
                e("42702", "column reference \"oid\" is ambiguous", 7)
            );
            assert_eq!(
                error("SELECT ctid FROM pg_am"),
                e("0A000", "the system column \"ctid\" is not supported yet", 7)
            );
            let s = |v: [&str; 4]| v.map(str::to_string);
            assert_eq!(
                full_error("SELECT amnam FROM pg_am"),
                s([
                    "42703",
                    "column \"amnam\" does not exist",
                    "",
                    "Perhaps you meant to reference the column \"pg_am.amname\"."
                ])
            );
            assert_eq!(
                full_error("SELECT pg_am.amname FROM pg_am a"),
                s([
                    "42P01",
                    "invalid reference to FROM-clause entry for table \"pg_am\"",
                    "",
                    "Perhaps you meant to reference the table alias \"a\"."
                ])
            );
            assert_eq!(
                full_error(
                    "SELECT 1 FROM pg_am a JOIN (pg_am b JOIN pg_am c ON a.oid = c.oid) ON true"
                ),
                s([
                    "42P01",
                    "invalid reference to FROM-clause entry for table \"a\"",
                    "There is an entry for table \"a\", but it cannot be referenced from this part of the query.",
                    ""
                ])
            );
            assert_eq!(
                full_error("SELECT a.amname FROM (pg_am a JOIN pg_am b USING (oid)) j"),
                s([
                    "42P01",
                    "invalid reference to FROM-clause entry for table \"a\"",
                    "There is an entry for table \"a\", but it cannot be referenced from this part of the query.",
                    ""
                ])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a, pg_am a"),
                s(["42712", "table name \"a\" specified more than once", "", ""])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a JOIN pg_am b USING (nosuch)"),
                s([
                    "42703",
                    "column \"nosuch\" specified in USING clause does not exist in left table",
                    "",
                    ""
                ])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a(x, y, z, w, v)"),
                s(["42P10", "table \"a\" has 4 columns available but 5 columns specified", "", ""])
            );
        });
    }
}

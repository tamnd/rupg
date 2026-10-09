//! Name resolution, type resolution, the query tree, the rewriter for views and rules.
//!
//! This crate first ships in milestone M2. See `spec/22-crate-layout.md` section 22.4 and `spec/23-milestones.md`.
//!
//! [`analyze`] takes a raw statement of `rupg-sql` and gives a [`Query`] whose expressions have their types, as `parse_analyze` of PostgreSQL does. The rules of type resolution come from `parse_coerce.c`, `parse_func.c` and `parse_oper.c` of PostgreSQL 19, so a query gets the same functions, the same casts and the same errors.

#![forbid(unsafe_code)]

mod agg;
mod coerce;
mod collate;
mod colname;
mod ddl;
mod expr;
mod from;
pub mod node;
mod poly;
mod prepare;
mod record;
mod resolve;
mod select;
mod setop;
mod sort;
mod sublink;
mod subscript;
mod transform;
mod typcache;
mod typename;
pub mod types;
mod values;
mod window;

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;
use rupg_types::{Value, oid};

use crate::coerce::AtOpt;

pub use coerce::{Context, Path, can_coerce, find_path};
pub use colname::figure_colname;
pub use ddl::{Body, Defined, Message, define, function_body, is_definition};
pub use expr::{
    Aggref, BoolOp, BoolTest, Case, CastForm, Expr, ExprKind, Func, FuncForm, SqlValue, SubLink,
    SubLinkKind, Subscript, Var, WindowFunc,
};
pub use from::{Column, FromFunction, FromItem, Join, JoinKind, Relation, TABLE_OID_ATTNUM};
pub use prepare::{execute_params, param_types};
pub use record::{out_columns, record_fields};
pub use select::{Query, Target};
pub use setop::{SetKind, SetOp, SetTree};
pub use sort::SortGroup;
pub use typcache::{default_opclass, sort_operators};
pub use typename::parse_type;
pub use window::WindowClause;

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
    /// The objects that the user made, or `None` when the session has no catalog.
    fn catalog(&self) -> Option<&rupg_catalog::Catalog> {
        None
    }
    /// The `allow_system_table_mods` setting, which lets a statement make a relation in a system schema. `initdb` turns it on.
    fn allow_system_table_mods(&self) -> bool {
        false
    }
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
    /// The scopes of the queries outside the subquery that the analyzer reads now, as `parentParseState`. The last is the query just outside it.
    outer: Vec<from::Scope>,
    /// The part of the query that the analyzer reads now.
    kind: agg::Kind,
    /// `p_hasAggs`: true when the query has an aggregate call.
    has_aggs: bool,
    /// `p_windowdefs`: the windows of the `WINDOW` clause of the query, then the windows of its `OVER` clauses.
    windowdefs: Vec<rupg_sql::nodes::WindowDef>,
    /// `p_hasWindowFuncs`: true when the query has a call of a window function.
    has_windows: bool,
    /// True when the next `SELECT` keeps its columns of type `unknown`, as `parse_sub_analyze` with no `resolve_unknowns` does for a part of a set operation.
    keep_unknowns: bool,
    /// The views whose queries the analyzer reads now, from the outermost. A view in its own query gives `42P17`, as `fireRIRrules` does.
    views: Vec<u32>,
    /// The base type and the typmod of the domain whose check constraint the analyzer reads now, or `None`. Then the name `value` is [`ExprKind::DomainValue`].
    domain_value: Option<(u32, i32)>,
    /// The number of the calls of functions that give a set that the analysis of the query has made, and the place of the last one. It takes the place of `p_last_srf`: `CASE` and `COALESCE` compare the number before and after their arguments.
    srfs: (usize, Option<usize>),
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
            outer: Vec::new(),
            kind: agg::Kind::Other,
            has_aggs: false,
            windowdefs: Vec::new(),
            has_windows: false,
            keep_unknowns: false,
            views: Vec::new(),
            domain_value: None,
            srfs: (0, None),
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

/// The query of a view of the catalog, from the text of the view and the `search_path` that the view keeps, or `None` when the catalog has no view with the OID. `pg_get_viewdef` shows this query.
pub fn view_query(env: &dyn Env, oid: u32) -> Result<Option<Query>> {
    Analyzer::new(env, &Params::default()).view_query(oid)
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
    pub(crate) struct TestEnv;

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
    fn order_by_errors() {
        big_stack(|| {
            let e = |state: &str, message: &str, at: usize| {
                (state.to_string(), message.to_string(), Some(at))
            };
            assert_eq!(
                error("SELECT oid FROM pg_am ORDER BY 2"),
                e("42P10", "ORDER BY position 2 is not in select list", 31)
            );
            assert_eq!(
                error("SELECT oid FROM pg_am ORDER BY 'x'"),
                e("42601", "non-integer constant in ORDER BY", 31)
            );
            assert_eq!(
                error("SELECT amname AS a, oid AS a FROM pg_am ORDER BY a"),
                e("42702", "ORDER BY \"a\" is ambiguous", 49)
            );
            assert_eq!(
                error("SELECT point(1, 2) AS p ORDER BY p"),
                e("42883", "could not identify an ordering operator for type point", 33)
            );
            assert_eq!(
                error("SELECT DISTINCT amname FROM pg_am ORDER BY oid"),
                e(
                    "42P10",
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list",
                    43
                )
            );
            assert_eq!(
                error("SELECT DISTINCT ON (oid) amname FROM pg_am ORDER BY amname"),
                e(
                    "42P10",
                    "SELECT DISTINCT ON expressions must match initial ORDER BY expressions",
                    20
                )
            );
            assert_eq!(
                error("SELECT amname FROM pg_am LIMIT oid"),
                e("42P10", "argument of LIMIT must not contain variables", 31)
            );
            assert_eq!(
                error("SELECT amname FROM pg_am LIMIT true"),
                e("42804", "argument of LIMIT must be type bigint, not type boolean", 31)
            );
            assert_eq!(
                error("SELECT amname FROM pg_am ORDER BY amname USING ="),
                e("42809", "operator = is not a valid ordering operator", 47)
            );
            let (state, message, _) =
                error("SELECT amname FROM pg_am ORDER BY 1 FETCH FIRST NULL ROWS WITH TIES");
            assert_eq!(
                (state.as_str(), message.as_str()),
                ("2201W", "row count cannot be null in FETCH FIRST ... WITH TIES clause")
            );
        });
    }

    #[test]
    fn order_by_clauses() {
        big_stack(|| {
            let query = run(
                "SELECT amname FROM pg_am ORDER BY amtype DESC NULLS LAST, 1, amname",
                &Params::default(),
            )
            .unwrap();
            // amtype is a junk column. The second amname is the same item as the first.
            assert_eq!(query.targets.len(), 2);
            assert!(!query.targets[0].junk && query.targets[1].junk);
            let items: Vec<_> =
                query.sort.iter().map(|s| (s.target, s.operator, s.nulls_first)).collect();
            assert_eq!(items, [(1, 633, false), (0, 660, false)]);
            let query = run(
                "SELECT DISTINCT ON (amtype) amtype, amname FROM pg_am ORDER BY amtype, oid",
                &Params::default(),
            )
            .unwrap();
            assert!(query.distinct_on);
            assert_eq!(query.distinct.iter().map(|s| s.target).collect::<Vec<_>>(), [0]);
            assert_eq!(query.sort.iter().map(|s| s.target).collect::<Vec<_>>(), [0, 2]);
            let query =
                run("SELECT DISTINCT amname, 1 FROM pg_am LIMIT 2 OFFSET 1", &Params::default())
                    .unwrap();
            assert_eq!(query.distinct.iter().map(|s| s.target).collect::<Vec<_>>(), [0, 1]);
            assert_eq!(query.limit.map(|e| e.ty), Some(oid::INT8));
            assert_eq!(query.offset.map(|e| e.ty), Some(oid::INT8));
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
            // A subquery in `FROM` with no `LATERAL` does not see the parts of `FROM` before it. The hint names `LATERAL` only where `LATERAL` could make the name visible.
            let entry = "There is an entry for table \"a\", but it cannot be referenced from this part of the query.";
            let lateral = "To reference that table, you must mark this subquery with LATERAL.";
            let invalid = "invalid reference to FROM-clause entry for table \"a\"";
            assert_eq!(
                full_error("SELECT s.* FROM pg_am a, (SELECT a.amname) s"),
                s(["42P01", invalid, entry, lateral])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a JOIN (SELECT a.amname) s ON true"),
                s(["42P01", invalid, entry, lateral])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a FULL JOIN (SELECT a.amname) s ON true"),
                s(["42P01", invalid, entry, ""])
            );
            // With `LATERAL`, the left side of a full or a right join is visible, but a reference to it is an error.
            let combining =
                "The combining JOIN type must be INNER or LEFT for a LATERAL reference.";
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a FULL JOIN LATERAL (SELECT a.amname) s ON true"),
                s(["42P10", invalid, combining, ""])
            );
            assert_eq!(
                full_error("SELECT 1 FROM pg_am a RIGHT JOIN LATERAL (SELECT amname) s ON true"),
                s(["42P10", invalid, combining, ""])
            );
            assert_eq!(
                full_error("SELECT s.* FROM pg_am a, (SELECT amname) s"),
                s([
                    "42703",
                    "column \"amname\" does not exist",
                    "There is a column named \"amname\" in table \"a\", but it cannot be referenced from this part of the query.",
                    "To reference that column, you must mark this subquery with LATERAL."
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

    #[test]
    fn aggregate_errors() {
        big_stack(|| {
            // The errors and the positions come from PostgreSQL 19.
            let cases = [
                ("SELECT sum(*) FROM pg_class", "42883", "function sum() does not exist", 7),
                (
                    "SELECT lower(DISTINCT relname) FROM pg_class",
                    "42809",
                    "DISTINCT specified, but lower is not an aggregate function",
                    7,
                ),
                (
                    "SELECT count(*) FROM pg_class WHERE count(*) > 1",
                    "42803",
                    "aggregate functions are not allowed in WHERE",
                    36,
                ),
                (
                    "SELECT 1 FROM pg_class JOIN pg_namespace n ON count(*) > 0",
                    "42803",
                    "aggregate functions are not allowed in JOIN conditions",
                    46,
                ),
                (
                    "SELECT count(*) FROM pg_class GROUP BY count(*)",
                    "42803",
                    "aggregate functions are not allowed in GROUP BY",
                    39,
                ),
                (
                    "SELECT count(*) FROM pg_class LIMIT count(*)",
                    "42803",
                    "aggregate functions are not allowed in LIMIT",
                    36,
                ),
                (
                    "SELECT sum(count(*)) FROM pg_class",
                    "42803",
                    "aggregate function calls cannot be nested",
                    11,
                ),
                (
                    "SELECT count(*) FILTER (WHERE count(*) > 1) FROM pg_class",
                    "42803",
                    "aggregate functions are not allowed in FILTER",
                    30,
                ),
                (
                    "SELECT relname, count(*) FROM pg_class",
                    "42803",
                    "column \"pg_class.relname\" must appear in the GROUP BY clause or be used in an aggregate function",
                    7,
                ),
                (
                    "SELECT c.relname FROM pg_class c GROUP BY c.relkind",
                    "42803",
                    "column \"c.relname\" must appear in the GROUP BY clause or be used in an aggregate function",
                    7,
                ),
                (
                    "SELECT mode(relnatts) FROM pg_class",
                    "42809",
                    "WITHIN GROUP is required for ordered-set aggregate mode",
                    7,
                ),
                (
                    "SELECT count(*) WITHIN GROUP (ORDER BY relname) FROM pg_class",
                    "42809",
                    "count is not an ordered-set aggregate, so it cannot have WITHIN GROUP",
                    7,
                ),
                (
                    "SELECT count(DISTINCT relname ORDER BY relkind) FROM pg_class",
                    "42P10",
                    "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
                    39,
                ),
                (
                    "SELECT count() FROM pg_class",
                    "42809",
                    "count(*) must be used to call a parameterless aggregate function",
                    7,
                ),
                (
                    "SELECT now(*) FROM pg_class",
                    "42809",
                    "now(*) specified, but now is not an aggregate function",
                    7,
                ),
                (
                    "SELECT count(*) FILTER (WHERE 1) FROM pg_class",
                    "42804",
                    "argument of FILTER must be type boolean, not type integer",
                    30,
                ),
                (
                    "SELECT relkind FROM pg_class GROUP BY relkind HAVING relnatts > 1",
                    "42803",
                    "column \"pg_class.relnatts\" must appear in the GROUP BY clause or be used in an aggregate function",
                    53,
                ),
                (
                    "SELECT relkind AS relname FROM pg_class GROUP BY relname",
                    "42803",
                    "column \"pg_class.relkind\" must appear in the GROUP BY clause or be used in an aggregate function",
                    7,
                ),
                (
                    "SELECT count(*) AS c FROM pg_class GROUP BY 1",
                    "42803",
                    "aggregate functions are not allowed in GROUP BY",
                    7,
                ),
                (
                    "SELECT n.nspname, c.relname FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace GROUP BY n.oid",
                    "42803",
                    "column \"c.relname\" must appear in the GROUP BY clause or be used in an aggregate function",
                    18,
                ),
                (
                    "SELECT oid FROM pg_class HAVING true",
                    "42803",
                    "column \"pg_class.oid\" must appear in the GROUP BY clause or be used in an aggregate function",
                    7,
                ),
                (
                    "SELECT array_agg(DISTINCT relacl::text::point) FROM pg_class",
                    "42883",
                    "could not identify an equality operator for type point",
                    26,
                ),
                (
                    "SELECT upper(relname) FROM pg_class GROUP BY lower(relname)",
                    "42803",
                    "column \"pg_class.relname\" must appear in the GROUP BY clause or be used in an aggregate function",
                    13,
                ),
            ];
            for (sql, state, message, at) in cases {
                assert_eq!(error(sql), (state.to_string(), message.to_string(), Some(at)), "{sql}");
            }
        });
    }

    #[test]
    fn subquery_errors() {
        big_stack(|| {
            // The errors and the positions come from PostgreSQL 19.
            let cases = [
                ("SELECT (SELECT 1, 2)", "42601", "subquery must return only one column", 7),
                ("SELECT 1 IN (SELECT 1, 2)", "42601", "subquery has too many columns", 9),
                ("SELECT 1 IN (SELECT FROM pg_am)", "42601", "subquery has too few columns", 9),
                (
                    "SELECT (SELECT c.amname) FROM pg_am c GROUP BY c.amtype",
                    "42803",
                    "subquery uses ungrouped column \"c.amname\" from outer query",
                    15,
                ),
                (
                    "SELECT 1 = ANY (SELECT 'a'::text)",
                    "42883",
                    "operator does not exist: integer = text",
                    9,
                ),
                (
                    "SELECT 1 + ANY (SELECT 1)",
                    "42804",
                    "row comparison operator must yield type boolean, not type integer",
                    9,
                ),
                (
                    "SELECT (SELECT nosuch FROM pg_am) FROM pg_class",
                    "42703",
                    "column \"nosuch\" does not exist",
                    15,
                ),
                (
                    "SELECT (SELECT x.oid FROM pg_am) FROM pg_class c",
                    "42P01",
                    "missing FROM-clause entry for table \"x\"",
                    15,
                ),
                (
                    "SELECT 1 FROM pg_am LIMIT (SELECT oid FROM pg_am a2 WHERE a2.oid = pg_am.oid)",
                    "42P10",
                    "argument of LIMIT must not contain variables",
                    67,
                ),
            ];
            for (sql, state, message, at) in cases {
                assert_eq!(error(sql), (state.to_string(), message.to_string(), Some(at)), "{sql}");
            }
            assert_eq!(
                error("SELECT ARRAY(SELECT NULL::pg_node_tree)"),
                (
                    "42704".to_string(),
                    "could not find array type for data type pg_node_tree".to_string(),
                    None
                )
            );
            // The hint of a close name searches the relations of the outer queries too.
            assert_eq!(
                full_error("SELECT (SELECT amnam FROM pg_am) FROM pg_class")[3],
                "Perhaps you meant to reference the column \"pg_am.amname\"."
            );
            assert_eq!(
                full_error("SELECT (SELECT relnam FROM pg_am) FROM pg_class")[3],
                "Perhaps you meant to reference the column \"pg_class.relname\" or the column \"pg_class.relam\"."
            );
            // A column of an outer query is a constant for LIMIT of the subquery.
            let query = run(
                "SELECT (SELECT a2.amname FROM pg_am a2 LIMIT a.oid::int8) FROM pg_am a",
                &Params::default(),
            )
            .expect("an outer column in LIMIT");
            let ExprKind::SubLink(sub) = &query.targets[0].expr.kind else { panic!("no SubLink") };
            assert!(sub.query.limit.as_ref().and_then(Expr::first_var).is_none());
        });
    }

    #[test]
    fn groups() {
        big_stack(|| {
            let ok =
                |sql: &str| run(sql, &Params::default()).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            let query =
                ok("SELECT relkind, count(*) FROM pg_class GROUP BY relkind HAVING count(*) > 1");
            assert!(query.grouped && query.having.is_some());
            assert_eq!(query.group.iter().map(|g| g.target).collect::<Vec<_>>(), [0]);
            // oid is the primary key of pg_class, so relname depends on it.
            assert!(ok("SELECT relname FROM pg_class GROUP BY oid").grouped);
            assert!(ok("SELECT lower(relname) FROM pg_class GROUP BY lower(relname)").grouped);
            assert!(ok("SELECT count(relname ORDER BY relname, relkind) FROM pg_class").grouped);
            assert!(ok("SELECT 1 HAVING true").grouped);
            assert!(ok("SELECT count(*) FROM pg_class GROUP BY ()").group.is_empty());
            assert!(!ok("SELECT relname FROM pg_class").grouped);
        });
    }
}

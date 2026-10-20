//! The parser of the body of a function, as the grammar `pl_gram.y` and the compiler `pl_comp.c` read it. The parser reads the tokens of [`Scanner`] by recursive descent and builds the tree of [`crate::ast`]. With the check of the syntax, the parser of SQL reads each expression and each statement of SQL, but the analysis of the queries waits for the first call.

use rupg_analyze::{Env, types};
use rupg_common::{Error, Level, Result, SqlState};
use rupg_sql::token;
use rupg_types::oid;

use crate::ast::{
    Block, Cond, Datum, DatumKind, DiagItem, Exceptions, Expr, Function, Handler, LabelKind, Mode,
    NsEntry, NsItem, RaiseOption, Returned, Source, Stmt, StmtKind, Target, ns_lookup,
};
use crate::scan::{Scanner, Tok};

/// An argument of a function.
#[derive(Clone, Debug)]
pub(crate) struct Arg {
    /// The name, or an empty name.
    pub(crate) name: String,
    /// The type, with a polymorphic type changed to the actual type.
    pub(crate) ty: u32,
    /// `proargmodes`: `i`, `o`, `b`, `v` or `t`.
    pub(crate) mode: u8,
}

/// The facts of a function that the compiler reads, as `do_compile` reads them from `pg_proc`.
#[derive(Clone, Debug)]
pub(crate) struct Signature {
    /// The name in the context of a compile error, `plpgsql_error_funcname`.
    pub(crate) name: String,
    pub(crate) args: Vec<Arg>,
    /// The result type, with a polymorphic type changed to the actual type.
    pub(crate) rettype: u32,
    pub(crate) retset: bool,
    /// True for a call of a trigger function as a trigger, `PLPGSQL_DML_TRIGGER`.
    pub(crate) trigger: bool,
}

/// The variables of a trigger function after `new` and `old`, with their types, in the order of `do_compile`.
pub(crate) const TRIGGER_VARIABLES: [(&str, u32); 10] = [
    ("tg_name", oid::NAME),
    ("tg_when", oid::TEXT),
    ("tg_level", oid::TEXT),
    ("tg_op", oid::TEXT),
    ("tg_relid", oid::OID),
    ("tg_relname", oid::NAME),
    ("tg_table_name", oid::NAME),
    ("tg_table_schema", oid::NAME),
    ("tg_nargs", oid::INT4),
    ("tg_argv", oid::TEXT_ARRAY),
];

/// `do_compile` and `plpgsql_compile_inline`: the function of the body `src`. With `check`, as `plpgsql_check_syntax` sets it, the parser of SQL reads each expression and statement.
///
/// # Errors
///
/// A syntax error at a byte offset in `src`. An error with no place gets the context of the line, as `plpgsql_compile_error_callback` adds it.
pub(crate) fn compile(src: &str, env: &dyn Env, sig: &Signature, check: bool) -> Result<Function> {
    let mut c = Compiler {
        sc: Scanner::new(src),
        env,
        check,
        datums: Vec::new(),
        recfields: Vec::new(),
        ns: Vec::new(),
        top: None,
        exprs: 0,
        retset: sig.retset,
        rettype: sig.rettype,
        out: None,
        latest: 0,
    };
    let result = c.function(sig);
    result.map_err(|e| {
        if e.position().is_some() {
            return e;
        }
        let line = c.sc.line(c.latest);
        e.with_context(format!(
            "compilation of PL/pgSQL function \"{}\" near line {line}",
            sig.name
        ))
    })
}

/// A word as `plpgsql_yylex` gives it: one to three names, which can name a variable.
#[derive(Debug)]
struct Word {
    /// The names, as `A`, `A.B` or `A.B.C`.
    names: Vec<String>,
    start: usize,
    end: usize,
    /// `T_DATUM`: the variable that the names name, with the field of a record.
    datum: Option<(usize, Option<String>)>,
    /// True for one word that is an unreserved keyword of PL/pgSQL.
    keyword: bool,
}

impl Word {
    /// `NameOfDatum`: the names as the text shows them.
    fn name(&self) -> String {
        self.names.join(".")
    }
}

/// The variable of a `FOR` loop, `for_variable`.
#[derive(Debug)]
struct ForVar {
    name: String,
    start: usize,
    /// A variable that is not a row or a record.
    scalar: Option<usize>,
    /// A row or a record, or a list of variables.
    row: Option<usize>,
}

/// The state of the compiler, as the globals of `pl_comp.c` keep it.
struct Compiler<'a> {
    sc: Scanner<'a>,
    env: &'a dyn Env,
    check: bool,
    datums: Vec<Datum>,
    /// The fields of records that the body names, which `plpgsql_build_recfield` makes datums of. Only the count matters, for the name of the variable of a `CASE`.
    recfields: Vec<(usize, String)>,
    ns: Vec<NsItem>,
    /// The last item of the names, `plpgsql_ns_top`.
    top: Option<usize>,
    exprs: usize,
    retset: bool,
    rettype: u32,
    /// `out_param_varno`.
    out: Option<usize>,
    /// The byte offset of the last line that the compiler counted, as `plpgsql_latest_lineno` keeps it.
    latest: usize,
}

/// True for a stop token of [`Compiler::read_sql`]: a character, `..`, or a keyword.
fn stops_at(t: &Tok, stop: &str) -> bool {
    match stop.as_bytes() {
        b".." => t.kind == token::DOT_DOT,
        [c] if !c.is_ascii_alphabetic() => t.is_char(*c),
        _ => t.is(stop),
    }
}

/// `scanner_isspace`.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The error of a feature that the compiler does not have yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// A syntax error at a place.
fn syntax_at(message: impl Into<String>, at: usize) -> Error {
    Error::new(SqlState::SYNTAX_ERROR, message).at(at)
}

/// True for an assignment operator, `=` or `:=`.
fn is_assign(t: &Tok) -> bool {
    t.is_char(b'=') || t.kind == token::COLON_EQUALS
}

impl Compiler<'_> {
    /// `plpgsql_location_to_lineno`: the line of a byte offset, which becomes the line of a later error with no place.
    fn lineno(&mut self, at: usize) -> u32 {
        self.latest = at;
        self.sc.line(at)
    }

    /// `plpgsql_yyerror`: a syntax error at the token `t`.
    fn yyerror(&self, t: &Tok, message: &str) -> Error {
        if t.is_end() {
            return syntax_at(format!("{message} at end of input"), t.start);
        }
        syntax_at(format!("{message} at or near \"{}\"", t.source(self.sc.src)), t.start)
    }

    /// The next token, which must be the keyword `word`.
    fn expect(&mut self, word: &str) -> Result<Tok> {
        let t = self.sc.next()?;
        if !t.is(word) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(t)
    }

    /// The next token, which must be the character `c`.
    fn expect_char(&mut self, c: u8) -> Result<Tok> {
        let t = self.sc.next()?;
        if !t.is_char(c) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(t)
    }

    /// `plpgsql_adddatum`.
    fn add_datum(&mut self, name: impl Into<String>, kind: DatumKind) -> usize {
        self.datums.push(Datum {
            name: name.into(),
            kind,
            constant: false,
            not_null: false,
            default: None,
            line: 0,
        });
        self.datums.len() - 1
    }

    /// `plpgsql_ns_additem`.
    fn ns_add(&mut self, name: impl Into<String>, entry: NsEntry) {
        self.ns.push(NsItem { prev: self.top, name: name.into(), entry });
        self.top = Some(self.ns.len() - 1);
    }

    /// `plpgsql_ns_push`: the start of the names of a block or a loop.
    fn ns_push(&mut self, label: Option<&str>, kind: LabelKind) {
        self.ns_add(label.unwrap_or_default(), NsEntry::Label(kind));
    }

    /// `plpgsql_ns_pop`: the end of the names of the last block or loop.
    fn ns_pop(&mut self) {
        while let Some(at) = self.top {
            let item = &self.ns[at];
            self.top = item.prev;
            if matches!(item.entry, NsEntry::Label(_)) {
                break;
            }
        }
    }

    /// True for a variable that is `PLPGSQL_NSTYPE_VAR`, which is not a record.
    fn is_var(&self, d: usize) -> bool {
        !matches!(self.datums[d].kind, DatumKind::Rec { .. })
    }

    /// `plpgsql_ns_lookup` from the last name: see [`ns_lookup`].
    fn lookup(
        &self,
        local: bool,
        n1: &str,
        n2: Option<&str>,
        n3: Option<&str>,
    ) -> Option<(usize, usize)> {
        let names: Vec<&str> = [n1].into_iter().chain(n2).chain(n3).collect();
        ns_lookup(&self.ns, &self.datums, self.top, local, &names)
    }

    /// `plpgsql_ns_lookup_label`: the kind of the block or loop with the label `name`.
    fn lookup_label(&self, name: &str) -> Option<LabelKind> {
        let mut cur = self.top;
        while let Some(at) = cur {
            if let NsEntry::Label(kind) = self.ns[at].entry
                && self.ns[at].name == name
            {
                return Some(kind);
            }
            cur = self.ns[at].prev;
        }
        None
    }

    /// `plpgsql_ns_find_nearest_loop`.
    fn in_loop(&self) -> bool {
        let mut cur = self.top;
        while let Some(at) = cur {
            if self.ns[at].entry == NsEntry::Label(LabelKind::Loop) {
                return true;
            }
            cur = self.ns[at].prev;
        }
        false
    }

    /// `plpgsql_build_recfield`: a field of a record that the body names.
    fn recfield(&mut self, rec: usize, field: &str) -> Option<String> {
        if !self.recfields.iter().any(|(r, f)| *r == rec && f == field) {
            self.recfields.push((rec, field.to_owned()));
        }
        Some(field.to_owned())
    }

    /// `plpgsql_yylex` for the token `t`, which the scanner passed: the word of `t` and the names after it that dots join, or `None` for a token that is not a word. A single word at the start of a statement, `start`, names a variable only before `=`, `:=` or `[`.
    fn word(&mut self, t: &Tok, start: bool) -> Result<Option<Word>> {
        let first = if t.kind == token::PARAM {
            format!("${}", t.text.as_deref().unwrap_or_default())
        } else if t.is_ident() {
            t.word.clone().unwrap_or_default()
        } else {
            return Ok(None);
        };
        let mut w = Word {
            names: vec![first],
            start: t.start,
            end: t.end,
            datum: None,
            keyword: t.kind != token::PARAM && t.is_unreserved(),
        };
        while w.names.len() < 3 && self.sc.peek()?.is_char(b'.') && self.sc.peek_at(1)?.is_ident() {
            self.sc.next()?;
            let n = self.sc.next()?;
            w.end = n.end;
            w.names.push(n.word.unwrap_or_default());
        }
        match w.names.len() {
            1 => {
                let next = self.sc.peek()?;
                let lookup = !start || is_assign(&next) || next.is_char(b'[');
                if lookup && let Some((d, _)) = self.lookup(false, &w.names[0], None, None) {
                    w.datum = Some((d, None));
                }
            }
            2 => {
                w.keyword = false;
                if let Some((d, used)) = self.lookup(false, &w.names[0], Some(&w.names[1]), None) {
                    let field = if !self.is_var(d) && used == 1 {
                        let name = w.names[1].clone();
                        self.recfield(d, &name)
                    } else {
                        None
                    };
                    w.datum = Some((d, field));
                }
            }
            _ => {
                w.keyword = false;
                let found = self.lookup(false, &w.names[0], Some(&w.names[1]), Some(&w.names[2]));
                if let Some((d, used)) = found
                    && !self.is_var(d)
                {
                    let name = if used == 1 { w.names[1].clone() } else { w.names[2].clone() };
                    let field = self.recfield(d, &name);
                    w.datum = Some((d, field));
                }
            }
        }
        Ok(Some(w))
    }

    /// `current_token_is_not_variable`: the error of a word that is not a variable, where a variable must be.
    fn not_variable(&self, t: &Tok, w: Option<&Word>) -> Error {
        match w {
            Some(w) if w.datum.is_none() && !w.keyword => {
                syntax_at(format!("\"{}\" is not a known variable", w.name()), t.start)
            }
            _ => self.yyerror(t, "syntax error"),
        }
    }

    /// `check_assignable`: `22005` for a variable that is `CONSTANT`.
    fn check_assignable(&self, d: usize, at: usize) -> Result<()> {
        let datum = &self.datums[d];
        match &datum.kind {
            DatumKind::Row { fields } => {
                for &f in fields {
                    self.check_assignable(f, at)?;
                }
                Ok(())
            }
            _ if datum.constant => Err(Error::new(
                SqlState::ERROR_IN_ASSIGNMENT,
                format!("variable \"{}\" is declared CONSTANT", datum.name),
            )
            .at(at)),
            _ => Ok(()),
        }
    }

    /// `plpgsql_build_variable` for a type: a record for `record` and a row type, a scalar for another type, and `None` for a pseudo-type.
    fn datum_kind(&self, ty: u32, typmod: i32) -> Option<DatumKind> {
        if ty == oid::RECORD || rupg_analyze::row_type_fields(self.env, ty).is_some() {
            return Some(DatumKind::Rec { ty });
        }
        if types::row(ty).is_some_and(|t| t.kind == b'p') {
            return None;
        }
        Some(DatumKind::Var { ty, typmod })
    }

    /// The function: the arguments, the variable `found`, the options and the top block.
    fn function(&mut self, sig: &Signature) -> Result<Function> {
        self.ns_push(Some(&sig.name), LabelKind::Block);
        let trigger = if sig.trigger { Some(self.trigger_datums(sig)?) } else { None };
        let (mut args, mut outs) = (Vec::new(), Vec::new());
        for (i, arg) in sig.args.iter().enumerate() {
            let dollar = format!("${}", i + 1);
            let Some(kind) = self.datum_kind(arg.ty, -1) else {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("PL/pgSQL functions cannot accept type {}", types::name(arg.ty)),
                ));
            };
            let name = if arg.name.is_empty() { dollar.clone() } else { arg.name.clone() };
            let d = self.add_datum(name, kind);
            if matches!(arg.mode, b'i' | b'b' | b'v') {
                args.push(d);
            }
            if matches!(arg.mode, b'o' | b'b' | b't') {
                outs.push(d);
            }
            self.parameter_name(&dollar, d)?;
            if !arg.name.is_empty() {
                self.parameter_name(&arg.name, d)?;
            }
        }
        self.out = match outs.as_slice() {
            [] => None,
            [one] => Some(*one),
            _ => Some(self.add_datum("(unnamed row)", DatumKind::Row { fields: outs.clone() })),
        };
        let rettype = sig.rettype;
        if types::row(rettype).is_some_and(|t| t.kind == b'p')
            && !sig.trigger
            && rettype != oid::VOID
            && rettype != oid::RECORD
        {
            if rettype == oid::TRIGGER || rettype == oid::EVENT_TRIGGER {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "trigger functions can only be called as triggers",
                ));
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("PL/pgSQL functions cannot return type {}", types::name(rettype)),
            ));
        }
        let found = self.add_datum("found", DatumKind::Var { ty: oid::BOOL, typmod: -1 });
        self.ns_add("found", NsEntry::Datum(found));
        self.options()?;
        let label = self.opt_block_label()?;
        let (body, _) = self.block(label)?;
        if self.sc.peek()?.is_char(b';') {
            self.sc.next()?;
        }
        let t = self.sc.next()?;
        if !t.is_end() {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(Function {
            datums: std::mem::take(&mut self.datums),
            ns: std::mem::take(&mut self.ns),
            args,
            found,
            out: self.out,
            body,
            exprs: self.exprs,
            retset: self.retset,
            rettype: self.rettype,
            trigger,
        })
    }

    /// The variables of a trigger function, as `do_compile` adds them for `PLPGSQL_DML_TRIGGER`: the records `new` and `old`, then [`TRIGGER_VARIABLES`]. It gives the variable `new`, and the others come after it in order.
    fn trigger_datums(&mut self, sig: &Signature) -> Result<usize> {
        if !sig.args.is_empty() {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "trigger functions cannot have declared arguments",
            )
            .with_hint(
                "The arguments of the trigger can be accessed through TG_NARGS and TG_ARGV instead.",
            ));
        }
        let new = self.add_datum("new", DatumKind::Rec { ty: oid::RECORD });
        self.ns_add("new", NsEntry::Datum(new));
        let old = self.add_datum("old", DatumKind::Rec { ty: oid::RECORD });
        self.ns_add("old", NsEntry::Datum(old));
        for (name, ty) in TRIGGER_VARIABLES {
            let d = self.add_datum(name, DatumKind::Var { ty, typmod: -1 });
            self.ns_add(name, NsEntry::Datum(d));
        }
        Ok(new)
    }

    /// `add_parameter_name`.
    fn parameter_name(&mut self, name: &str, d: usize) -> Result<()> {
        if self.lookup(true, name, None, None).is_some() {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                format!("parameter name \"{name}\" used more than once"),
            ));
        }
        self.ns_add(name, NsEntry::Datum(d));
        Ok(())
    }

    /// `comp_options`: the lines `#option dump`, `#print_strict_params` and `#variable_conflict` before the top block.
    fn options(&mut self) -> Result<()> {
        while self.sc.peek()?.is_op("#") {
            self.sc.next()?;
            let t = self.sc.next()?;
            if t.is("option") {
                self.expect("dump")?;
            } else if t.is("print_strict_params") {
                let v = self.sc.next()?;
                if !v.is_ident() {
                    return Err(self.yyerror(&v, "syntax error"));
                }
            } else if t.is("variable_conflict") {
                let v = self.sc.next()?;
                if v.is("use_variable") || v.is("use_column") {
                    return Err(not_yet("#variable_conflict with a value other than error"));
                }
                if !v.is("error") {
                    return Err(self.yyerror(&v, "syntax error"));
                }
            } else {
                return Err(self.yyerror(&t, "syntax error"));
            }
        }
        Ok(())
    }

    /// `any_identifier`: a name, which is a word, an unreserved keyword or a variable.
    fn any_identifier(&mut self) -> Result<(String, usize)> {
        let t = self.sc.next()?;
        match self.word(&t, false)? {
            Some(w) if w.names.len() == 1 => Ok((w.names[0].clone(), t.start)),
            _ => Err(self.yyerror(&t, "syntax error")),
        }
    }

    /// `opt_label`: the label after `END` or `EXIT`, if any.
    fn opt_label(&mut self) -> Result<Option<(String, usize)>> {
        let t = self.sc.peek()?;
        if t.is_ident() || t.kind == token::PARAM {
            return self.any_identifier().map(Some);
        }
        Ok(None)
    }

    /// `opt_block_label`: `<<label>>` before a block or a loop.
    fn opt_block_label(&mut self) -> Result<Option<String>> {
        if !self.sc.peek()?.is_op("<<") {
            return Ok(None);
        }
        self.sc.next()?;
        let (label, _) = self.any_identifier()?;
        let t = self.sc.next()?;
        if !t.is_op(">>") {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(Some(label))
    }

    /// `check_labels`: the label after `END` must be the label of the block or loop.
    fn check_labels(&self, start: Option<&str>, end: Option<(String, usize)>) -> Result<()> {
        let Some((end, at)) = end else { return Ok(()) };
        match start {
            None => {
                Err(syntax_at(format!("end label \"{end}\" specified for unlabeled block"), at))
            }
            Some(start) if start != end => Err(syntax_at(
                format!("end label \"{end}\" differs from block's label \"{start}\""),
                at,
            )),
            Some(_) => Ok(()),
        }
    }

    /// A new expression of the text `text` at the byte offset `at`.
    fn expr(&mut self, text: String, mode: Mode, at: usize) -> Expr {
        self.exprs += 1;
        Expr {
            id: self.exprs - 1,
            shown: text.clone(),
            text,
            lead: 0,
            mode,
            ns: self.top,
            at,
            target: None,
        }
    }

    /// `check_sql_expr`: the parser of SQL reads the text, and a syntax error gets the place in the body.
    fn check_sql(&self, text: &str, mode: Mode, at: usize) -> Result<()> {
        if !self.check {
            return Ok(());
        }
        let result = match mode {
            Mode::Statement => rupg_sql::parse(text).map(drop),
            Mode::Expr | Mode::Assign => rupg_sql::parse_plpgsql_expr(text).map(drop),
        };
        result.map_err(|e| {
            let inner = e.location.unwrap_or(0);
            Error::from(e).with_position(Some(at + inner))
        })
    }

    /// The next token of a text of SQL, where the names that dots join are one token, as `plpgsql_yylex` reads them in `IDENTIFIER_LOOKUP_EXPR`. The names of a field of a record make the field.
    fn sql_token(&mut self) -> Result<Tok> {
        let mut t = self.sc.next()?;
        let compound = (t.is_ident() || t.kind == token::PARAM)
            && self.sc.peek()?.is_char(b'.')
            && self.sc.peek_at(1)?.is_ident();
        if compound && let Some(w) = self.word(&t, false)? {
            t.end = w.end;
            t.kind = token::IDENT;
            t.word = None;
        }
        Ok(t)
    }

    /// `read_sql_construct`: the text up to one of `stops` at the level 0 of the parentheses, with the token that ends it. `expression` chooses the words of the errors.
    fn read_sql(
        &mut self,
        stops: &[&str],
        expected: &str,
        mode: Mode,
        expression: bool,
        check: bool,
    ) -> Result<(Expr, Tok)> {
        let mut level = 0;
        let mut first = None;
        let mut end = None;
        let last = loop {
            let t = self.sql_token()?;
            first.get_or_insert(t.start);
            if level == 0 && stops.iter().any(|s| stops_at(&t, s)) {
                break t;
            }
            if t.is_char(b'(') || t.is_char(b'[') {
                level += 1;
            } else if t.is_char(b')') || t.is_char(b']') {
                level -= 1;
                if level < 0 {
                    return Err(self.yyerror(&t, "mismatched parentheses"));
                }
            }
            // The end of the body in a query is a syntax error at the end of the input, as PostgreSQL gives it.
            if t.is_end() {
                return Err(self.yyerror(&t, "syntax error"));
            }
            if t.is_char(b';') {
                if level != 0 {
                    return Err(self.yyerror(&t, "mismatched parentheses"));
                }
                let what = if expression { "expression" } else { "statement" };
                return Err(syntax_at(
                    format!("missing \"{expected}\" at end of SQL {what}"),
                    t.start,
                ));
            }
            end = Some(t.end);
        };
        let start = first.unwrap_or(last.start);
        let Some(end) = end.filter(|&end| end > start) else {
            let what = if expression { "missing expression" } else { "missing SQL statement" };
            return Err(self.yyerror(&last, what));
        };
        let text = self.sc.src[start..end].to_owned();
        if check {
            self.check_sql(&text, mode, start)?;
        }
        Ok((self.expr(text, mode, start), last))
    }

    /// `read_sql_expression`: an expression up to one of `stops`.
    fn expr_until(&mut self, stops: &[&str], expected: &str) -> Result<(Expr, Tok)> {
        self.read_sql(stops, expected, Mode::Expr, true, true)
    }

    /// `read_sql_stmt`: a statement of SQL up to `;`.
    fn sql_stmt(&mut self) -> Result<Expr> {
        Ok(self.read_sql(&[";"], ";", Mode::Statement, false, true)?.0)
    }

    /// `pl_block`, after its label: `[DECLARE decls] BEGIN stmts [EXCEPTION handlers] END [label]`. It gives the block and the place of `BEGIN`.
    fn block(&mut self, label: Option<String>) -> Result<(Block, usize)> {
        self.ns_push(label.as_deref(), LabelKind::Block);
        let mut decls = Vec::new();
        if self.sc.peek()?.is("declare") {
            self.sc.next()?;
            loop {
                let t = self.sc.peek()?;
                if t.is("begin") {
                    break;
                }
                if t.is("declare") {
                    self.sc.next()?;
                } else if t.is_op("<<") {
                    return Err(syntax_at(
                        "block label must be placed before DECLARE, not after",
                        t.start,
                    ));
                } else {
                    self.decl(&mut decls)?;
                }
            }
        }
        let begin = self.expect("begin")?;
        let body = self.stmts()?;
        let exceptions = if self.sc.peek()?.is("exception") {
            let t = self.sc.next()?;
            self.lineno(t.start);
            Some(self.exceptions()?)
        } else {
            None
        };
        self.expect("end")?;
        let end = self.opt_label()?;
        self.check_labels(label.as_deref(), end)?;
        self.ns_pop();
        Ok((Block { label, decls, body, exceptions }, begin.start))
    }

    /// `exception_sect`: the variables `sqlstate` and `sqlerrm`, and the handlers.
    fn exceptions(&mut self) -> Result<Exceptions> {
        let text = DatumKind::Var { ty: oid::TEXT, typmod: -1 };
        let sqlstate = self.add_datum("sqlstate", text);
        self.datums[sqlstate].constant = true;
        self.ns_add("sqlstate", NsEntry::Datum(sqlstate));
        let text = DatumKind::Var { ty: oid::TEXT, typmod: -1 };
        let sqlerrm = self.add_datum("sqlerrm", text);
        self.datums[sqlerrm].constant = true;
        self.ns_add("sqlerrm", NsEntry::Datum(sqlerrm));
        let mut handlers = Vec::new();
        loop {
            let t = self.expect("when")?;
            let conds = self.conditions()?;
            self.expect("then")?;
            let body = self.stmts()?;
            self.lineno(t.start);
            handlers.push(Handler { conds, body });
            if !self.sc.peek()?.is("when") {
                break;
            }
        }
        Ok(Exceptions { sqlstate, sqlerrm, handlers })
    }

    /// `proc_conditions`: the conditions of a handler, which `OR` joins.
    fn conditions(&mut self) -> Result<Vec<Cond>> {
        let mut conds = Vec::new();
        loop {
            let (name, _) = self.any_identifier()?;
            if name == "sqlstate" {
                let s = self.sc.next()?;
                if s.kind != token::SCONST {
                    return Err(self.yyerror(&s, "syntax error"));
                }
                let code = s.text.clone().unwrap_or_default();
                let Some(state) = SqlState::parse(&code) else {
                    return Err(self.yyerror(&s, "invalid SQLSTATE code"));
                };
                conds.push(Cond::State(state));
            } else if name == "others" {
                conds.push(Cond::Others);
            } else {
                let before = conds.len();
                conds.extend(SqlState::for_condition(&name).map(Cond::State));
                if conds.len() == before {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("unrecognized exception condition \"{name}\""),
                    ));
                }
            }
            if !self.sc.peek()?.is("or") {
                return Ok(conds);
            }
            self.sc.next()?;
        }
    }

    /// `decl_statement`: a variable, an alias or a cursor.
    fn decl(&mut self, decls: &mut Vec<usize>) -> Result<()> {
        let t = self.sc.next()?;
        if !t.is_ident() {
            return Err(self.yyerror(&t, "syntax error"));
        }
        if self.sc.peek()?.is_char(b'.') && self.sc.peek_at(1)?.is_ident() {
            self.sc.next()?;
            let n = self.sc.next()?;
            let text = &self.sc.src[t.start..n.end];
            return Err(syntax_at(format!("syntax error at or near \"{text}\""), t.start));
        }
        let name = t.word.clone().unwrap_or_default();
        let line = self.lineno(t.start);
        if self.lookup(true, &name, None, None).is_some() {
            return Err(self.yyerror(&t, "duplicate declaration"));
        }
        let next = self.sc.peek()?;
        if next.is("alias") {
            self.sc.next()?;
            self.expect("for")?;
            let item = self.alias_item()?;
            self.expect_char(b';')?;
            self.ns_add(name, NsEntry::Datum(item));
            return Ok(());
        }
        if next.is("cursor") || next.is("scroll") || next.is("no") {
            return Err(not_yet("a cursor in PL/pgSQL"));
        }
        let constant = next.is("constant");
        if constant {
            self.sc.next()?;
        }
        let (ty, typmod) = self.datatype()?;
        if self.sc.peek()?.is("collate") {
            let c = self.sc.next()?;
            let n = self.sc.next()?;
            if self.word(&n, false)?.is_none() {
                return Err(self.yyerror(&n, "syntax error"));
            }
            if !types::collatable(ty) {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("collations are not supported by type {}", types::name(ty)),
                )
                .at(c.start));
            }
        }
        let not_null = if self.sc.peek()?.is("not") {
            let n = self.sc.next()?;
            self.expect("null")?;
            Some(n.start)
        } else {
            None
        };
        let t = self.sc.next()?;
        let default = if t.is_char(b';') {
            None
        } else if is_assign(&t) || t.is("default") {
            Some(self.expr_until(&[";"], ";")?.0)
        } else {
            return Err(self.yyerror(&t, "syntax error"));
        };
        let Some(kind) = self.datum_kind(ty, typmod) else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("variable \"{name}\" has pseudo-type {}", types::name(ty)),
            ));
        };
        let d = self.add_datum(name.clone(), kind);
        self.ns_add(name.clone(), NsEntry::Datum(d));
        decls.push(d);
        if let Some(at) = not_null
            && default.is_none()
        {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!(
                    "variable \"{name}\" must have a default value, since it's declared NOT NULL"
                ),
            )
            .at(at));
        }
        let datum = &mut self.datums[d];
        datum.constant = constant;
        datum.not_null = not_null.is_some();
        datum.default = default;
        datum.line = line;
        Ok(())
    }

    /// `decl_aliasitem`: the variable of `ALIAS FOR`.
    fn alias_item(&mut self) -> Result<usize> {
        let t = self.sc.next()?;
        let mut names = match t.kind {
            token::PARAM => vec![format!("${}", t.text.as_deref().unwrap_or_default())],
            _ if t.is_ident() => vec![t.word.clone().unwrap_or_default()],
            _ => return Err(self.yyerror(&t, "syntax error")),
        };
        while names.len() < 3 && self.sc.peek()?.is_char(b'.') && self.sc.peek_at(1)?.is_ident() {
            self.sc.next()?;
            names.push(self.sc.next()?.word.unwrap_or_default());
        }
        let found = match names.as_slice() {
            [n1] => self.lookup(false, n1, None, None),
            [n1, n2] => self.lookup(false, n1, Some(n2), None),
            [n1, n2, n3] => self.lookup(false, n1, Some(n2), Some(n3)),
            _ => None,
        };
        let Some((d, _)) = found else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("variable \"{}\" does not exist", names.join(".")),
            )
            .at(t.start));
        };
        Ok(d)
    }

    /// `read_datatype`: the type of a declaration, as `name%TYPE`, `relation%ROWTYPE` or a type name.
    fn datatype(&mut self) -> Result<(u32, i32)> {
        let place = self.sc.place();
        let t = self.sc.next()?;
        if t.is_ident() {
            let mut names = vec![t.word.clone().unwrap_or_default()];
            while names.len() < 3 && self.sc.peek()?.is_char(b'.') && self.sc.peek_at(1)?.is_ident()
            {
                self.sc.next()?;
                names.push(self.sc.next()?.word.unwrap_or_default());
            }
            let percent = self.sc.peek()?;
            if percent.is_char(b'%') || percent.is_op("%") {
                self.sc.next()?;
                let k = self.sc.next()?;
                let found = if k.is("type") {
                    Some(self.wordtype(&names)?)
                } else if k.is("rowtype") {
                    Some((self.rowtype(&names)?, -1))
                } else {
                    None
                };
                if let Some(found) = found {
                    return self.array_decoration(found);
                }
            }
        }
        self.sc.reset(place);
        let start = t.start;
        let mut level = 0;
        loop {
            let t = self.sc.peek()?;
            if t.is_char(b';') {
                break;
            }
            if t.is_end() {
                let what = if level != 0 {
                    "mismatched parentheses"
                } else {
                    "incomplete data type declaration"
                };
                return Err(self.yyerror(&t, what));
            }
            if t.is("collate") || t.is("not") || is_assign(&t) || t.is("default") {
                break;
            }
            if (t.is_char(b',') || t.is_char(b')')) && level == 0 {
                break;
            }
            if t.is_char(b'(') {
                level += 1;
            } else if t.is_char(b')') {
                level -= 1;
            }
            self.sc.next()?;
        }
        let stop = self.sc.peek()?;
        let text = &self.sc.src[start..stop.start];
        if text.is_empty() {
            return Err(self.yyerror(&stop, "missing data type declaration"));
        }
        let place = |e: Error| {
            // `parse_type` counts characters, and the place here is a byte offset.
            let inner = e
                .position()
                .map_or(0, |chars| text.char_indices().nth(chars).map_or(text.len(), |(b, _)| b));
            e.with_position(Some(start + inner))
        };
        rupg_analyze::parse_type(text, self.env).map_err(place)?.map_err(place)
    }

    /// The array decoration after `%TYPE` and `%ROWTYPE`: `[ARRAY] [ '[' [n] ']' ... ]`.
    fn array_decoration(&mut self, found: (u32, i32)) -> Result<(u32, i32)> {
        let mut array = false;
        if self.sc.peek()?.is("array") {
            self.sc.next()?;
            array = true;
        }
        while self.sc.peek()?.is_char(b'[') {
            self.sc.next()?;
            array = true;
            if self.sc.peek()?.kind == token::ICONST {
                self.sc.next()?;
            }
            let t = self.sc.next()?;
            if !t.is_char(b']') {
                return Err(self.yyerror(&t, "syntax error, expected \"]\""));
            }
        }
        if !array {
            return Ok(found);
        }
        match types::array_of(found.0) {
            0 => Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("could not find array type for data type {}", types::name(found.0)),
            )),
            ty => Ok((ty, found.1)),
        }
    }

    /// `plpgsql_parse_wordtype` and `plpgsql_parse_cwordtype`: the type of a variable or of a column.
    fn wordtype(&self, names: &[String]) -> Result<(u32, i32)> {
        let of = |d: usize| match self.datums[d].kind {
            DatumKind::Var { ty, typmod } => Some((ty, typmod)),
            DatumKind::Rec { ty } => Some((ty, -1)),
            DatumKind::Row { .. } => None,
        };
        let (relation, column) = match names {
            [name] => {
                let found = self.lookup(false, name, None, None).and_then(|(d, _)| of(d));
                return found.ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("variable \"{name}\" does not exist"),
                    )
                });
            }
            [n1, n2] => {
                if let Some((d, used)) = self.lookup(false, n1, Some(n2), None)
                    && (self.is_var(d) || used == 2)
                    && let Some(found) = of(d)
                {
                    return Ok(found);
                }
                (&names[..1], n2)
            }
            _ => (&names[..names.len() - 1], &names[names.len() - 1]),
        };
        let relation: Vec<&str> = relation.iter().map(String::as_str).collect();
        let relname = relation.last().copied().unwrap_or_default();
        if rupg_analyze::relation_by_names(self.env, &relation)?.is_none() {
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", relation.join(".")),
            ));
        }
        rupg_analyze::column_type(self.env, &relation, column)?.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{relname}\" does not exist"),
            )
        })
    }

    /// `plpgsql_parse_wordrowtype` and `plpgsql_parse_cwordrowtype`: the row type of a relation.
    fn rowtype(&self, names: &[String]) -> Result<u32> {
        let relation: Vec<&str> = names.iter().map(String::as_str).collect();
        if rupg_analyze::relation_by_names(self.env, &relation)?.is_none() {
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{}\" does not exist", relation.join(".")),
            ));
        }
        rupg_analyze::relation_row_type(self.env, &relation)?.ok_or_else(|| {
            Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!(
                    "relation \"{}\" does not have a composite type",
                    relation.last().copied().unwrap_or_default()
                ),
            )
        })
    }

    /// `proc_sect`: the statements up to a word that ends them.
    fn stmts(&mut self) -> Result<Vec<Stmt>> {
        let mut list = Vec::new();
        loop {
            let t = self.sc.peek()?;
            let ends = ["end", "else", "elsif", "elseif", "when", "exception"];
            if t.is_end() || ends.iter().any(|w| t.is(w)) {
                return Ok(list);
            }
            list.push(self.stmt()?);
        }
    }

    /// A statement with the line of the place `at`, which the compiler counts after the statement, as the actions of the grammar do.
    fn finish(&mut self, at: usize, kind: StmtKind) -> Stmt {
        Stmt { line: self.lineno(at), kind }
    }

    /// `proc_stmt`.
    fn stmt(&mut self) -> Result<Stmt> {
        let t = self.sc.peek()?;
        if t.is_op("<<") {
            let label = self.opt_block_label()?;
            let k = self.sc.peek()?;
            if k.is("begin") || k.is("declare") {
                return self.block_stmt(label);
            }
            self.sc.next()?;
            return match k.word.as_deref() {
                Some("loop" | "while" | "for" | "foreach") if !k.quoted => {
                    self.loop_stmt(label, &k)
                }
                _ => Err(self.yyerror(&k, "syntax error")),
            };
        }
        if t.is("begin") || t.is("declare") {
            return self.block_stmt(None);
        }
        let t = self.sc.next()?;
        if t.is_reserved() {
            return match t.word.as_deref().unwrap_or_default() {
                "if" => self.if_stmt(&t),
                "case" => self.case_stmt(&t),
                "loop" | "while" | "for" | "foreach" => self.loop_stmt(None, &t),
                "null" => {
                    self.expect_char(b';')?;
                    Ok(self.finish(t.start, StmtKind::Null))
                }
                _ => Err(self.yyerror(&t, "syntax error")),
            };
        }
        let Some(w) = self.word(&t, true)? else {
            return Err(self.yyerror(&t, "syntax error"));
        };
        if w.datum.is_some() {
            return self.assign(w);
        }
        if w.keyword {
            return match w.names[0].as_str() {
                "exit" => self.exit(&t, true),
                "continue" => self.exit(&t, false),
                "return" => self.return_stmt(&t),
                "raise" => self.raise(&t),
                "assert" => self.assert(&t),
                "execute" => self.execute(&t),
                "perform" => self.perform(&t),
                "get" => self.getdiag(&t),
                "call" | "do" => {
                    self.sc.back();
                    let expr = self.sql_stmt()?;
                    Ok(self.finish(t.start, StmtKind::Call { expr, call: t.is("call") }))
                }
                "insert" | "import" | "merge" => self.execsql(&t),
                "open" | "fetch" | "move" | "close" => Err(not_yet("a cursor in PL/pgSQL")),
                "commit" | "rollback" => Err(not_yet("transaction control in PL/pgSQL")),
                _ => Err(self.yyerror(&t, "syntax error")),
            };
        }
        let next = self.sc.peek()?;
        if is_assign(&next) || next.is_char(b'[') || next.is_char(b'.') {
            return Err(syntax_at(format!("\"{}\" is not a known variable", w.name()), t.start));
        }
        self.execsql(&t)
    }

    /// `stmt_block`: a block and `;`.
    fn block_stmt(&mut self, label: Option<String>) -> Result<Stmt> {
        let (block, begin) = self.block(label)?;
        self.expect_char(b';')?;
        Ok(self.finish(begin, StmtKind::Block(block)))
    }

    /// `stmt_assign`: `target := expr`, where the target can have a field or subscripts.
    fn assign(&mut self, w: Word) -> Result<Stmt> {
        let Some((d, field)) = w.datum.clone() else { return Err(not_yet("this assignment")) };
        self.check_assignable(d, w.start)?;
        let mut subscripts = Vec::new();
        while self.sc.peek()?.is_char(b'[') {
            self.sc.next()?;
            subscripts.push(self.expr_until(&["]"], "]")?.0);
        }
        let target = match (field, subscripts.is_empty()) {
            (None, true) => Target::Datum(d),
            (None, false) => Target::Element(d, subscripts),
            (Some(field), true) => Target::Field(d, field),
            (Some(_), false) => return Err(not_yet("an assignment to an element of a field")),
        };
        let op = self.sc.next()?;
        if !is_assign(&op) {
            return Err(self.yyerror(&op, "syntax error"));
        }
        let (mut expr, _) = self.read_sql(&[";"], ";", Mode::Assign, true, true)?;
        let stop = expr.at + expr.text.len();
        expr.shown = self.sc.src[w.start..stop].to_owned();
        expr.lead = expr.at - w.start;
        if let Target::Datum(d) = target {
            expr.target = Some(d);
        }
        Ok(self.finish(w.start, StmtKind::Assign { target, expr }))
    }

    /// `stmt_if`.
    fn if_stmt(&mut self, t: &Tok) -> Result<Stmt> {
        let cond = self.expr_until(&["then"], "THEN")?.0;
        let then = self.stmts()?;
        let mut elsifs = Vec::new();
        loop {
            let e = self.sc.peek()?;
            if !(e.is("elsif") || e.is("elseif")) {
                break;
            }
            self.sc.next()?;
            let cond = self.expr_until(&["then"], "THEN")?.0;
            let body = self.stmts()?;
            self.lineno(e.start);
            elsifs.push((cond, body));
        }
        let otherwise = if self.sc.peek()?.is("else") {
            self.sc.next()?;
            Some(self.stmts()?)
        } else {
            None
        };
        self.expect("end")?;
        self.expect("if")?;
        self.expect_char(b';')?;
        Ok(self.finish(t.start, StmtKind::If { cond, then, elsifs, otherwise }))
    }

    /// `stmt_case` and `make_case`. A `CASE` with a test value keeps it in a new variable, and each `WHEN` becomes `"var" IN (values)`.
    fn case_stmt(&mut self, t: &Tok) -> Result<Stmt> {
        let test = if self.sc.peek()?.is("when") {
            None
        } else {
            let (test, _) = self.expr_until(&["when"], "WHEN")?;
            self.sc.back();
            Some(test)
        };
        let mut whens = Vec::new();
        while self.sc.peek()?.is("when") {
            let w = self.sc.next()?;
            let (cond, _) = self.expr_until(&["then"], "THEN")?;
            let body = self.stmts()?;
            self.lineno(w.start);
            whens.push((cond, body));
        }
        if whens.is_empty() {
            let n = self.sc.next()?;
            return Err(self.yyerror(&n, "syntax error"));
        }
        let otherwise = if self.sc.peek()?.is("else") {
            self.sc.next()?;
            Some(self.stmts()?)
        } else {
            None
        };
        self.expect("end")?;
        self.expect("case")?;
        self.expect_char(b';')?;
        let test = match test {
            None => None,
            Some(test) => {
                let name =
                    format!("__Case__Variable_{}__", self.datums.len() + self.recfields.len());
                let var = self.add_datum(name.clone(), DatumKind::Var { ty: 0, typmod: -1 });
                self.ns_add(name.clone(), NsEntry::Datum(var));
                for (cond, _) in &mut whens {
                    cond.text = format!("\"{name}\" IN ({})", cond.text);
                    cond.shown = cond.text.clone();
                    cond.ns = self.top;
                }
                Some((test, var))
            }
        };
        Ok(self.finish(t.start, StmtKind::Case { test, whens, otherwise }))
    }

    /// `loop_body`: the statements, `END LOOP [label];`.
    fn loop_body(&mut self, label: Option<&str>) -> Result<Vec<Stmt>> {
        let body = self.stmts()?;
        self.expect("end")?;
        self.expect("loop")?;
        let end = self.opt_label()?;
        self.expect_char(b';')?;
        self.check_labels(label, end)?;
        Ok(body)
    }

    /// `stmt_loop`, `stmt_while`, `stmt_for` and `stmt_foreach_a`, after the keyword `k`.
    fn loop_stmt(&mut self, label: Option<String>, k: &Tok) -> Result<Stmt> {
        self.ns_push(label.as_deref(), LabelKind::Loop);
        let kind = match k.word.as_deref().unwrap_or_default() {
            "loop" => {
                let body = self.loop_body(label.as_deref())?;
                StmtKind::Loop { label, body }
            }
            "while" => {
                let cond = self.expr_until(&["loop"], "LOOP")?.0;
                let body = self.loop_body(label.as_deref())?;
                StmtKind::While { label, cond, body }
            }
            "for" => self.for_control(label)?,
            _ => self.foreach(label)?,
        };
        self.ns_pop();
        Ok(self.finish(k.start, kind))
    }

    /// `for_variable`: a variable, a list of variables, or a new name.
    fn for_variable(&mut self) -> Result<ForVar> {
        let t = self.sc.next()?;
        let Some(w) = self.word(&t, false)? else {
            return Err(self.yyerror(&t, "syntax error"));
        };
        let mut var = ForVar { name: w.name(), start: t.start, scalar: None, row: None };
        match &w.datum {
            Some((d, field)) => {
                if field.is_some() {
                    return Err(not_yet("a field of a record as a loop variable"));
                }
                if !matches!(self.datums[*d].kind, DatumKind::Var { .. }) {
                    var.row = Some(*d);
                } else {
                    var.scalar = Some(*d);
                    if self.sc.peek()?.is_char(b',') {
                        var.row = Some(self.scalar_list(*d, w.start)?);
                    }
                }
            }
            None if w.keyword => return Err(self.yyerror(&t, "syntax error")),
            None if w.names.len() > 1 => {
                return Err(syntax_at(
                    format!("\"{}\" is not a known variable", w.name()),
                    t.start,
                ));
            }
            None => {
                if self.sc.peek()?.is_char(b',') {
                    return Err(syntax_at(
                        format!("\"{}\" is not a known variable", w.name()),
                        t.start,
                    ));
                }
            }
        }
        Ok(var)
    }

    /// The target of a loop over rows: a row, a record, or a list of one variable, `make_scalar_list1`.
    fn rows_target(&mut self, var: &ForVar, state: SqlState) -> Result<usize> {
        if let Some(row) = var.row {
            self.check_assignable(row, var.start)?;
            return Ok(row);
        }
        if let Some(scalar) = var.scalar {
            self.check_assignable(scalar, var.start)?;
            return Ok(self.add_datum(var.name.clone(), DatumKind::Row { fields: vec![scalar] }));
        }
        Err(Error::new(
            state,
            "loop variable of loop over rows must be a record variable or list of scalar variables",
        )
        .at(var.start))
    }

    /// `for_control` and the body of `stmt_for`.
    fn for_control(&mut self, label: Option<String>) -> Result<StmtKind> {
        let var = self.for_variable()?;
        self.expect("in")?;
        let place = self.sc.place();
        let t = self.sc.next()?;
        if t.is("execute") {
            let (query, term) = self.expr_until(&["loop", "using"], "LOOP or USING")?;
            let target = self.rows_target(&var, SqlState::DATATYPE_MISMATCH)?;
            let mut params = Vec::new();
            if term.is("using") {
                loop {
                    let (param, term) = self.expr_until(&[",", "loop"], ", or LOOP")?;
                    params.push(param);
                    if !term.is_char(b',') {
                        break;
                    }
                }
            }
            let body = self.loop_body(label.as_deref())?;
            let source = Source::Dynamic { query, params };
            return Ok(StmtKind::ForQuery { label, target, source, body });
        }
        if let Some(w) = self.word(&t, false)?
            && let Some((d, None)) = w.datum
            && matches!(self.datums[d].kind, DatumKind::Var { ty: oid::REFCURSOR, .. })
        {
            return Err(not_yet("a loop over a cursor"));
        }
        let reverse = t.is("reverse");
        self.sc.reset(place);
        if reverse {
            self.sc.next()?;
        }
        let (mut lower, term) =
            self.read_sql(&["..", "loop"], "LOOP", Mode::Statement, true, false)?;
        if term.kind == token::DOT_DOT {
            lower.mode = Mode::Expr;
            self.check_sql(&lower.text, Mode::Expr, lower.at)?;
            let (upper, term) = self.expr_until(&["loop", "by"], "LOOP")?;
            let step =
                if term.is("by") { Some(self.expr_until(&["loop"], "LOOP")?.0) } else { None };
            if var.scalar.is_some() && var.row.is_some() {
                return Err(syntax_at(
                    "integer FOR loop must have only one target variable",
                    var.start,
                ));
            }
            let int = DatumKind::Var { ty: oid::INT4, typmod: -1 };
            let d = self.add_datum(var.name.clone(), int);
            self.ns_add(var.name, NsEntry::Datum(d));
            let body = self.loop_body(label.as_deref())?;
            return Ok(StmtKind::ForInt { label, var: d, reverse, lower, upper, step, body });
        }
        if reverse {
            return Err(syntax_at("cannot specify REVERSE in query FOR loop", t.start));
        }
        self.check_sql(&lower.text, Mode::Statement, lower.at)?;
        let target = self.rows_target(&var, SqlState::SYNTAX_ERROR)?;
        let body = self.loop_body(label.as_deref())?;
        Ok(StmtKind::ForQuery { label, target, source: Source::Query(lower), body })
    }

    /// `stmt_foreach_a`: `FOREACH var [SLICE n] IN ARRAY expr LOOP`.
    fn foreach(&mut self, label: Option<String>) -> Result<StmtKind> {
        let var = self.for_variable()?;
        let slice = if self.sc.peek()?.is("slice") {
            self.sc.next()?;
            let n = self.sc.next()?;
            if n.kind != token::ICONST {
                return Err(self.yyerror(&n, "syntax error"));
            }
            n.text.as_deref().and_then(|n| n.parse().ok()).unwrap_or(0)
        } else {
            0
        };
        self.expect("in")?;
        self.expect("array")?;
        let expr = self.expr_until(&["loop"], "LOOP")?.0;
        let body = self.loop_body(label.as_deref())?;
        let Some(target) = var.row.or(var.scalar) else {
            return Err(syntax_at(
                "loop variable of FOREACH must be a known variable or list of variables",
                var.start,
            ));
        };
        self.check_assignable(target, var.start)?;
        Ok(StmtKind::ForEach { label, target, slice, expr, body })
    }

    /// `stmt_exit`: `EXIT` and `CONTINUE`, with a label and a condition.
    fn exit(&mut self, t: &Tok, exit: bool) -> Result<Stmt> {
        let label = self.opt_label()?;
        let cond = if self.sc.peek()?.is("when") {
            self.sc.next()?;
            Some(self.expr_until(&[";"], ";")?.0)
        } else {
            self.expect_char(b';')?;
            None
        };
        match &label {
            Some((name, at)) => match self.lookup_label(name) {
                None => {
                    return Err(syntax_at(
                        format!(
                            "there is no label \"{name}\" attached to any block or loop enclosing this statement"
                        ),
                        *at,
                    ));
                }
                Some(LabelKind::Block) if !exit => {
                    return Err(syntax_at(
                        format!("block label \"{name}\" cannot be used in CONTINUE"),
                        *at,
                    ));
                }
                Some(_) => {}
            },
            None if !self.in_loop() => {
                let message = if exit {
                    "EXIT cannot be used outside a loop, unless it has a label"
                } else {
                    "CONTINUE cannot be used outside a loop"
                };
                return Err(syntax_at(message, t.start));
            }
            None => {}
        }
        let label = label.map(|(name, _)| name);
        Ok(self.finish(t.start, StmtKind::Exit { exit, label, cond }))
    }

    /// The error of a value after `RETURN` where the function takes none.
    fn no_return_value(&mut self, message: &str, hint: Option<&str>) -> Result<()> {
        let n = self.sc.next()?;
        if n.is_char(b';') {
            return Ok(());
        }
        let error = Error::new(SqlState::DATATYPE_MISMATCH, message).at(n.start);
        Err(match hint {
            Some(hint) => error.with_hint(hint),
            None => error,
        })
    }

    /// The value of `RETURN` and `RETURN NEXT`: a variable alone, or an expression.
    fn return_value(&mut self) -> Result<Returned> {
        let place = self.sc.place();
        let t = self.sc.next()?;
        if let Some(w) = self.word(&t, false)?
            && let Some((d, None)) = w.datum
            && self.sc.peek()?.is_char(b';')
        {
            self.sc.next()?;
            return Ok(Returned::Var(d));
        }
        self.sc.reset(place);
        Ok(Returned::Expr(self.expr_until(&[";"], ";")?.0))
    }

    /// `stmt_return`: `RETURN`, `RETURN NEXT` and `RETURN QUERY`.
    fn return_stmt(&mut self, t: &Tok) -> Result<Stmt> {
        let n = self.sc.next()?;
        if n.is_end() {
            return Err(self.yyerror(&n, "unexpected end of function definition"));
        }
        if n.is("next") {
            if !self.retset {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot use RETURN NEXT in a non-SETOF function",
                )
                .at(t.start));
            }
            let value = match self.out {
                Some(out) => {
                    let message =
                        "RETURN NEXT cannot have a parameter in function with OUT parameters";
                    self.no_return_value(message, None)?;
                    Returned::Var(out)
                }
                None => self.return_value()?,
            };
            return Ok(self.finish(t.start, StmtKind::ReturnNext(value)));
        }
        if n.is("query") {
            if !self.retset {
                return Err(syntax_at("cannot use RETURN QUERY in a non-SETOF function", t.start));
            }
            let e = self.sc.next()?;
            let source = if e.is("execute") {
                let (query, params) = self.dynamic_query()?;
                Source::Dynamic { query, params }
            } else {
                self.sc.back();
                Source::Query(self.sql_stmt()?)
            };
            return Ok(self.finish(t.start, StmtKind::ReturnQuery(source)));
        }
        self.sc.back();
        let value = if self.retset {
            let message = "RETURN cannot have a parameter in function returning set";
            self.no_return_value(message, Some("Use RETURN NEXT or RETURN QUERY."))?;
            Returned::Nothing
        } else if self.rettype == oid::VOID {
            self.no_return_value(
                "RETURN cannot have a parameter in function returning void",
                None,
            )?;
            Returned::Nothing
        } else if let Some(out) = self.out {
            let message = "RETURN cannot have a parameter in function with OUT parameters";
            self.no_return_value(message, None)?;
            Returned::Var(out)
        } else {
            self.return_value()?
        };
        Ok(self.finish(t.start, StmtKind::Return(value)))
    }

    /// The query of `RETURN QUERY EXECUTE`: `expr [USING params];`.
    fn dynamic_query(&mut self) -> Result<(Expr, Vec<Expr>)> {
        let (query, term) = self.expr_until(&[";", "using"], "; or USING")?;
        let mut params = Vec::new();
        if term.is("using") {
            loop {
                let (param, term) = self.expr_until(&[",", ";"], ", or ;")?;
                params.push(param);
                if !term.is_char(b',') {
                    break;
                }
            }
        }
        Ok((query, params))
    }

    /// `stmt_raise`.
    fn raise(&mut self, t: &Tok) -> Result<Stmt> {
        let mut level = None;
        let mut condition = None;
        let mut condname = None;
        let mut message = None;
        let mut params = Vec::new();
        let mut options = Vec::new();
        let mut n = self.sc.next()?;
        if n.is_end() {
            return Err(self.yyerror(&n, "unexpected end of function definition"));
        }
        if !n.is_char(b';') {
            let levels = [
                ("exception", None),
                ("warning", Some(Level::Warning)),
                ("notice", Some(Level::Notice)),
                ("info", Some(Level::Info)),
                ("log", Some(Level::Log)),
                ("debug", Some(Level::Debug)),
            ];
            if let Some((_, l)) = levels.iter().find(|(w, _)| n.is(w)) {
                level = *l;
                n = self.sc.next()?;
            }
            if n.is_end() {
                return Err(self.yyerror(&n, "unexpected end of function definition"));
            }
            if n.kind == token::SCONST {
                message = n.text.clone();
                n = self.sc.next()?;
                if !(n.is_char(b',') || n.is_char(b';') || n.is("using")) {
                    return Err(self.yyerror(&n, "syntax error"));
                }
                while n.is_char(b',') {
                    let (param, term) = self.read_sql(
                        &[",", ";", "using"],
                        ", or ; or USING",
                        Mode::Expr,
                        true,
                        true,
                    )?;
                    params.push(param);
                    n = term;
                }
            } else if !n.is("using") {
                if n.is("sqlstate") {
                    let s = self.sc.next()?;
                    if s.kind != token::SCONST {
                        return Err(self.yyerror(&s, "syntax error"));
                    }
                    let code = s.text.clone().unwrap_or_default();
                    let Some(state) = SqlState::parse(&code) else {
                        return Err(self.yyerror(&s, "invalid SQLSTATE code"));
                    };
                    condition = Some(state);
                    condname = Some(code);
                } else {
                    if !n.is_ident() {
                        return Err(self.yyerror(&n, "syntax error"));
                    }
                    let name = n.word.clone().unwrap_or_default();
                    let Some(state) = SqlState::for_condition(&name).next() else {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("unrecognized exception condition \"{name}\""),
                        ));
                    };
                    condition = Some(state);
                    condname = Some(name);
                }
                n = self.sc.next()?;
                if !(n.is_char(b';') || n.is("using")) {
                    return Err(self.yyerror(&n, "syntax error"));
                }
            }
            if n.is("using") {
                options = self.raise_options()?;
            }
        }
        if let Some(message) = &message {
            let mut expected = 0;
            let mut bytes = message.bytes();
            while let Some(b) = bytes.next() {
                if b == b'%' {
                    if bytes.clone().next() == Some(b'%') {
                        bytes.next();
                    } else {
                        expected += 1;
                    }
                }
            }
            if expected < params.len() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "too many parameters specified for RAISE",
                ));
            }
            if expected > params.len() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "too few parameters specified for RAISE",
                ));
            }
        }
        let kind = StmtKind::Raise { level, condition, condname, message, params, options };
        Ok(self.finish(t.start, kind))
    }

    /// `read_raise_options`: `USING option = expr, ...`.
    fn raise_options(&mut self) -> Result<Vec<(RaiseOption, Expr)>> {
        let names = [
            ("errcode", RaiseOption::Errcode),
            ("message", RaiseOption::Message),
            ("detail", RaiseOption::Detail),
            ("hint", RaiseOption::Hint),
            ("column", RaiseOption::Column),
            ("constraint", RaiseOption::Constraint),
            ("datatype", RaiseOption::Datatype),
            ("table", RaiseOption::Table),
            ("schema", RaiseOption::Schema),
        ];
        let mut options = Vec::new();
        loop {
            let t = self.sc.next()?;
            if t.is_end() {
                return Err(self.yyerror(&t, "unexpected end of function definition"));
            }
            let Some((_, kind)) = names.iter().find(|(w, _)| t.is(w)) else {
                return Err(self.yyerror(&t, "unrecognized RAISE statement option"));
            };
            let op = self.sc.next()?;
            if !is_assign(&op) {
                return Err(self.yyerror(&op, "syntax error, expected \"=\""));
            }
            let (expr, term) = self.expr_until(&[",", ";"], ", or ;")?;
            options.push((*kind, expr));
            if term.is_char(b';') {
                return Ok(options);
            }
        }
    }

    /// `stmt_assert`: `ASSERT cond [, message];`.
    fn assert(&mut self, t: &Tok) -> Result<Stmt> {
        let (cond, term) = self.expr_until(&[",", ";"], ", or ;")?;
        let message = if term.is_char(b',') { Some(self.expr_until(&[";"], ";")?.0) } else { None };
        Ok(self.finish(t.start, StmtKind::Assert { cond, message }))
    }

    /// `stmt_dynexecute`: `EXECUTE expr [INTO [STRICT] target] [USING params];`, with `INTO` and `USING` in either order.
    fn execute(&mut self, t: &Tok) -> Result<Stmt> {
        let (query, mut term) =
            self.read_sql(&["into", "using", ";"], "INTO or USING or ;", Mode::Expr, true, true)?;
        let mut into = None;
        let mut strict = false;
        let mut params = Vec::new();
        loop {
            if term.is("into") {
                if into.is_some() {
                    return Err(self.yyerror(&term, "syntax error"));
                }
                let (target, s) = self.target_of_into(true)?;
                into = Some(target);
                strict = s;
                term = self.sc.next()?;
            } else if term.is("using") {
                if !params.is_empty() {
                    return Err(self.yyerror(&term, "syntax error"));
                }
                loop {
                    let (param, next) = self.read_sql(
                        &[",", ";", "into"],
                        ", or ; or INTO",
                        Mode::Expr,
                        true,
                        true,
                    )?;
                    params.push(param);
                    term = next;
                    if !term.is_char(b',') {
                        break;
                    }
                }
            } else if term.is_char(b';') {
                break;
            } else {
                return Err(self.yyerror(&term, "syntax error"));
            }
        }
        Ok(self.finish(t.start, StmtKind::Execute { query, into, strict, params }))
    }

    /// `stmt_perform`: the text of the statement with `SELECT` for `PERFORM`.
    fn perform(&mut self, t: &Tok) -> Result<Stmt> {
        self.sc.back();
        let (mut expr, _) = self.read_sql(&[";"], ";", Mode::Statement, false, false)?;
        expr.text = format!("SELECT{}", &self.sc.src[t.end..expr.at + expr.text.len()]);
        expr.shown = expr.text.clone();
        expr.at = t.start + 1;
        self.check_sql(&expr.text, Mode::Statement, expr.at)?;
        Ok(self.finish(t.start, StmtKind::Perform(expr)))
    }

    /// `stmt_getdiag`: `GET [CURRENT | STACKED] DIAGNOSTICS target = item, ...;`.
    fn getdiag(&mut self, t: &Tok) -> Result<Stmt> {
        let mut stacked = false;
        let area = self.sc.peek()?;
        if area.is("current") || area.is("stacked") {
            self.sc.next()?;
            stacked = area.is("stacked");
        }
        self.expect("diagnostics")?;
        let names = [
            ("row_count", DiagItem::RowCount),
            ("pg_routine_oid", DiagItem::RoutineOid),
            ("pg_context", DiagItem::Context),
            ("pg_exception_detail", DiagItem::ErrorDetail),
            ("pg_exception_hint", DiagItem::ErrorHint),
            ("pg_exception_context", DiagItem::ErrorContext),
            ("column_name", DiagItem::ColumnName),
            ("constraint_name", DiagItem::ConstraintName),
            ("pg_datatype_name", DiagItem::DatatypeName),
            ("message_text", DiagItem::MessageText),
            ("table_name", DiagItem::TableName),
            ("schema_name", DiagItem::SchemaName),
            ("returned_sqlstate", DiagItem::ReturnedSqlstate),
        ];
        let mut items = Vec::new();
        loop {
            let v = self.sc.next()?;
            let w = self.word(&v, false)?;
            let Some((d, field)) = w.as_ref().and_then(|w| w.datum.clone()) else {
                return Err(self.not_variable(&v, w.as_ref()));
            };
            if (!matches!(self.datums[d].kind, DatumKind::Var { .. }) && field.is_none())
                || self.sc.peek()?.is_char(b'[')
            {
                let name = w.map(|w| w.name()).unwrap_or_default();
                return Err(syntax_at(format!("\"{name}\" is not a scalar variable"), v.start));
            }
            if field.is_some() {
                return Err(not_yet("a field of a record as a target of GET DIAGNOSTICS"));
            }
            self.check_assignable(d, v.start)?;
            let op = self.sc.next()?;
            if !is_assign(&op) {
                return Err(self.yyerror(&op, "syntax error"));
            }
            let i = self.sc.next()?;
            let Some((_, item)) = names.iter().find(|(w, _)| i.is(w)) else {
                return Err(self.yyerror(&i, "unrecognized GET DIAGNOSTICS item"));
            };
            items.push((d, *item));
            if !self.sc.peek()?.is_char(b',') {
                break;
            }
            self.sc.next()?;
        }
        self.expect_char(b';')?;
        for (_, item) in &items {
            let area = if stacked && matches!(item, DiagItem::RowCount | DiagItem::RoutineOid) {
                "STACKED"
            } else if !stacked && item.stacked() {
                "CURRENT"
            } else {
                continue;
            };
            return Err(syntax_at(
                format!(
                    "diagnostics item {} is not allowed in GET {area} DIAGNOSTICS",
                    item.name()
                ),
                t.start,
            ));
        }
        Ok(self.finish(t.start, StmtKind::GetDiag { stacked, items }))
    }

    /// `read_into_target`: `[STRICT] target`, where the target is a row, a record, or a list of variables.
    fn target_of_into(&mut self, allow_strict: bool) -> Result<(usize, bool)> {
        let mut strict = false;
        let mut t = self.sc.next()?;
        if allow_strict && t.is("strict") {
            strict = true;
            t = self.sc.next()?;
        }
        let w = self.word(&t, false)?;
        let Some((d, field)) = w.as_ref().and_then(|w| w.datum.clone()) else {
            return Err(self.not_variable(&t, w.as_ref()));
        };
        if field.is_some() {
            return Err(not_yet("a field of a record as a target of INTO"));
        }
        if !matches!(self.datums[d].kind, DatumKind::Var { .. }) {
            self.check_assignable(d, t.start)?;
            let n = self.sc.peek()?;
            if n.is_char(b',') {
                return Err(syntax_at(
                    "record variable cannot be part of multiple-item INTO list",
                    n.start,
                ));
            }
            return Ok((d, strict));
        }
        Ok((self.scalar_list(d, t.start)?, strict))
    }

    /// `read_into_scalar_list`: a row of the variable `first` and the variables after it, which commas join.
    fn scalar_list(&mut self, first: usize, at: usize) -> Result<usize> {
        self.check_assignable(first, at)?;
        let mut fields = vec![first];
        while self.sc.peek()?.is_char(b',') {
            self.sc.next()?;
            let t = self.sc.next()?;
            let w = self.word(&t, false)?;
            let Some((d, field)) = w.as_ref().and_then(|w| w.datum.clone()) else {
                return Err(self.not_variable(&t, w.as_ref()));
            };
            if field.is_some() {
                return Err(not_yet("a field of a record as a target of INTO"));
            }
            self.check_assignable(d, t.start)?;
            if !matches!(self.datums[d].kind, DatumKind::Var { .. }) {
                let name = w.map(|w| w.name()).unwrap_or_default();
                return Err(syntax_at(format!("\"{name}\" is not a scalar variable"), t.start));
            }
            fields.push(d);
        }
        Ok(self.add_datum("(unnamed row)", DatumKind::Row { fields }))
    }

    /// `make_execsql_stmt`: a statement of SQL up to `;`, with the target of an `INTO` that the statement has. The text keeps spaces for the `INTO` part, so that the places of the text stay the places of the body.
    fn execsql(&mut self, first: &Tok) -> Result<Stmt> {
        let start = first.start;
        let mut lead = vec![first.word.clone().unwrap_or_default()];
        let mut routine = false;
        let (mut level, mut begins) = (0, 0);
        let mut prev_insert = first.is("insert") || first.is("merge");
        let import = first.is("import");
        let mut into: Option<(usize, Option<usize>)> = None;
        let mut target = None;
        let mut strict = false;
        let last = loop {
            let t = self.sql_token()?;
            if let Some((_, end)) = &mut into
                && end.is_none()
            {
                *end = Some(t.start);
            }
            if lead.len() < 4 {
                lead.push(if t.quoted {
                    String::new()
                } else {
                    t.word.clone().unwrap_or_default()
                });
                let words: Vec<&str> = lead.iter().map(String::as_str).collect();
                routine |= matches!(
                    words.as_slice(),
                    ["create", "function" | "procedure", ..]
                        | ["create", "or", "replace", "function" | "procedure"]
                );
            }
            if t.is_char(b'(') {
                level += 1;
            } else if t.is_char(b')') && level > 0 {
                level -= 1;
            }
            if routine && level == 0 {
                if t.is("begin") || t.is("case") {
                    begins += 1;
                } else if t.is("end") && begins > 0 {
                    begins -= 1;
                }
            }
            if t.is_char(b';') && level == 0 && begins == 0 {
                break t;
            }
            if t.is_end() {
                return Err(self.yyerror(&t, "unexpected end of function definition"));
            }
            if t.is("into") && !prev_insert && !import {
                if into.is_some() {
                    return Err(self.yyerror(&t, "INTO specified more than once"));
                }
                into = Some((t.start, None));
                let (tg, s) = self.target_of_into(true)?;
                target = Some(tg);
                strict = s;
            }
            prev_insert = t.is("insert") || t.is("merge");
        };
        let src = self.sc.src;
        let mut text = match into {
            Some((from, Some(to))) => {
                format!("{}{}{}", &src[start..from], " ".repeat(to - from), &src[to..last.start])
            }
            _ => src[start..last.start].to_owned(),
        };
        while text.as_bytes().last().is_some_and(|&b| is_space(b)) {
            text.pop();
        }
        self.check_sql(&text, Mode::Statement, start)?;
        let sql = self.expr(text, Mode::Statement, start);
        Ok(self.finish(start, StmtKind::ExecSql { sql, into: target, strict }))
    }
}

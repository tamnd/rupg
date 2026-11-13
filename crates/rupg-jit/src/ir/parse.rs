// Lifted from rudb crates/rudb-qc-ir/src/parse.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! Reads the text form back, for tests and for replaying a module captured from a failing
//! query.
//!
//! The parser builds the arena directly, with no folding, so that what it reads is what the
//! module holds. Blocks are read in a first pass over each function, so that a branch can name a
//! block below it and a constant argument can take its type from the parameter it lands in.
//! Values are numbered as they are first mentioned, which lets a use come before its definition
//! in the text: block layout puts cold blocks last, so that is normal.

use std::collections::HashMap;
use std::fmt;

use crate::ir::func::{A16, Const, DEAD, ErrorKind, Field, INV, NT};
use crate::ir::{
    Block, BlockData, CATALOGUE, Class, Form, Func, Module, Op, Site, Ty, Val, ValInfo,
};

/// Why a module did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based line.
    pub line: usize,
    /// What was wrong.
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Num(String),
    Val(String),
    Err(u32),
    Guard(u32),
    Ctr(u32),
    At(String),
    Str(String),
    Punct(&'static str),
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-'
}

fn lex(line: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let take = |i: &mut usize, ok: &dyn Fn(char) -> bool| {
        let start = *i;
        while *i < chars.len() && ok(chars[*i]) {
            *i += 1;
        }
        chars[start..*i].iter().collect::<String>()
    };
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => i += 1,
            ';' => break,
            '"' => {
                i += 1;
                let mut s = String::new();
                loop {
                    let Some(&c) = chars.get(i) else {
                        return Err("an unterminated string".into());
                    };
                    i += 1;
                    match c {
                        '"' => break,
                        '\\' => {
                            let e = *chars.get(i).ok_or("an unterminated escape")?;
                            i += 1;
                            match e {
                                'n' => s.push('\n'),
                                't' => s.push('\t'),
                                'r' => s.push('\r'),
                                '0' => s.push('\0'),
                                '\\' | '"' | '\'' => s.push(e),
                                'u' => {
                                    let hex = take(&mut i, &|c| c != '}');
                                    i += 1;
                                    let hex = hex.trim_start_matches('{');
                                    let code = u32::from_str_radix(hex, 16)
                                        .map_err(|_| "a bad \\u escape")?;
                                    s.push(char::from_u32(code).ok_or("a bad \\u escape")?);
                                }
                                _ => return Err(format!("an unknown escape \\{e}")),
                            }
                        }
                        _ => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            '%' => {
                i += 1;
                out.push(Tok::Val(take(&mut i, &|c| {
                    c.is_ascii_alphanumeric() || c == '_' || c == '.'
                })));
            }
            '@' => {
                i += 1;
                out.push(Tok::At(take(&mut i, &is_word)));
            }
            '!' | '#' => {
                i += 1;
                let kind = if c == '#' { ' ' } else { *chars.get(i).ok_or("a bare !")? };
                if c == '!' {
                    i += 1;
                }
                let n = take(&mut i, &|c| c.is_ascii_digit());
                let n: u32 = n.parse().map_err(|_| format!("a bad table reference after {c}"))?;
                out.push(match kind {
                    'E' => Tok::Err(n),
                    'G' => Tok::Guard(n),
                    ' ' => Tok::Ctr(n),
                    _ => return Err(format!("an unknown table !{kind}")),
                });
            }
            '-' if chars.get(i + 1) == Some(&'>') => {
                i += 2;
                out.push(Tok::Punct("->"));
            }
            '-' if chars.get(i + 1).is_some_and(char::is_ascii_digit) => {
                i += 1;
                let n = take(&mut i, &|c| c.is_ascii_alphanumeric() || c == '.');
                out.push(Tok::Num(format!("-{n}")));
            }
            '0'..='9' => {
                let mut n = take(&mut i, &|c| c.is_ascii_alphanumeric() || c == '.');
                // An exponent with a sign, as in 1e-5.
                if n.ends_with('e') && matches!(chars.get(i), Some('-' | '+')) {
                    n.push(chars[i]);
                    i += 1;
                    n.push_str(&take(&mut i, &|c| c.is_ascii_digit()));
                }
                out.push(Tok::Num(n));
            }
            '[' | ']' | '(' | ')' | ',' | ':' | '=' | '+' | '-' | '*' => {
                i += 1;
                out.push(Tok::Punct(match c {
                    '[' => "[",
                    ']' => "]",
                    '(' => "(",
                    ')' => ")",
                    ',' => ",",
                    ':' => ":",
                    '=' => "=",
                    '+' => "+",
                    '-' => "-",
                    _ => "*",
                }));
            }
            c if is_word(c) => out.push(Tok::Word(take(&mut i, &is_word))),
            _ => return Err(format!("an unexpected character {c:?}")),
        }
    }
    Ok(out)
}

/// Parses a constant of type `ty`.
fn konst(ty: Ty, s: &str) -> Result<u128, String> {
    let bad = || format!("{s:?} is not a {} constant", ty.name());
    if let Some(hex) = s.strip_prefix("0x") {
        return u128::from_str_radix(hex, 16).map_err(|_| bad());
    }
    Ok(match ty {
        Ty::I1 => match s {
            "true" => 1,
            "false" => 0,
            _ => return Err(bad()),
        },
        Ty::F64 => u128::from(s.parse::<f64>().map_err(|_| bad())?.to_bits()),
        Ty::F32 => u128::from(s.parse::<f32>().map_err(|_| bad())?.to_bits()),
        _ if s.starts_with('-') => s.parse::<i128>().map_err(|_| bad())? as u128,
        _ => s.parse::<u128>().map_err(|_| bad())?,
    } & ty.mask())
}

struct Cursor<'a> {
    toks: &'a [Tok],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Result<Tok, String> {
        let t = self.toks.get(self.at).cloned().ok_or("the line ends too early")?;
        self.at += 1;
        Ok(t)
    }

    fn punct(&mut self, p: &str) -> Result<(), String> {
        match self.next()? {
            Tok::Punct(q) if q == p => Ok(()),
            t => Err(format!("expected {p:?}, found {t:?}")),
        }
    }

    fn eat(&mut self, p: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Punct(q)) if *q == p);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn word(&mut self) -> Result<String, String> {
        match self.next()? {
            Tok::Word(w) => Ok(w),
            t => Err(format!("expected a word, found {t:?}")),
        }
    }

    fn num<T: std::str::FromStr>(&mut self) -> Result<T, String> {
        match self.next()? {
            Tok::Num(n) => n.parse().map_err(|_| format!("{n} is out of range")),
            t => Err(format!("expected a number, found {t:?}")),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        match self.next()? {
            Tok::Str(s) => Ok(s),
            t => Err(format!("expected a string, found {t:?}")),
        }
    }

    fn err(&mut self) -> Result<u32, String> {
        match self.next()? {
            Tok::Err(n) => Ok(n),
            t => Err(format!("expected an error site, found {t:?}")),
        }
    }

    fn block(&mut self) -> Result<u32, String> {
        let w = self.word()?;
        w.strip_prefix('b')
            .and_then(|n| n.parse().ok())
            .ok_or(format!("expected a block, found {w}"))
    }

    fn ty(&mut self) -> Result<Ty, String> {
        let w = self.word()?;
        Ty::from_name(&w).ok_or(format!("{w} is not a type"))
    }

    fn done(&self) -> Result<(), String> {
        match self.peek() {
            None => Ok(()),
            Some(t) => Err(format!("unexpected {t:?} at the end of the line")),
        }
    }
}

/// One function being read.
struct FuncReader {
    f: Func,
    names: HashMap<String, Val>,
    defined: Vec<bool>,
}

impl FuncReader {
    fn named(&mut self, name: &str) -> Val {
        if let Some(v) = self.names.get(name) {
            return *v;
        }
        self.f.vals.push(ValInfo {
            ty: Ty::Void,
            class: Class::Persistent,
            name: Some(name.into()),
        });
        self.defined.push(false);
        let v = Val((self.f.vals.len() - 1) as u32);
        self.names.insert(name.to_owned(), v);
        v
    }

    fn define(&mut self, name: &str, ty: Ty) -> Result<Val, String> {
        let v = self.named(name);
        if std::mem::replace(&mut self.defined[v.index()], true) {
            return Err(format!("%{name} is defined twice"));
        }
        self.f.vals[v.index()].ty = ty;
        Ok(v)
    }

    /// An operand whose type, if it is a constant, is `ty`.
    fn operand(&mut self, c: &mut Cursor<'_>, ty: Ty) -> Result<u32, String> {
        match c.next()? {
            Tok::Val(n) => Ok(self.named(&n).0),
            Tok::Num(n) => Ok(self.konst(ty, &n)?.0),
            Tok::Word(w) if w == "true" || w == "false" => Ok(self.konst(ty, &w)?.0),
            Tok::Word(w) if w == "none" => Ok(Val::NONE.0),
            t => Err(format!("expected an operand, found {t:?}")),
        }
    }

    fn konst(&mut self, ty: Ty, s: &str) -> Result<Val, String> {
        let bits = konst(ty, s)?;
        self.f.consts.push(Const { ty, bits });
        Ok(Val::konst((self.f.consts.len() - 1) as u32))
    }

    /// `bN` or `bN(args)`, with constant arguments typed by the block's parameters.
    fn target(&mut self, c: &mut Cursor<'_>, out: &mut Vec<u32>) -> Result<(u32, usize), String> {
        let b = c.block()?;
        let types: Vec<Ty> = match self.f.blocks.get(b as usize) {
            Some(data) => data.params.iter().map(|v| self.f.vals[v.index()].ty).collect(),
            None => return Err(format!("b{b} is not a block")),
        };
        let mut n = 0;
        if c.eat("(") {
            loop {
                let ty = *types.get(n).ok_or(format!("too many arguments to b{b}"))?;
                out.push(self.operand(c, ty)?);
                n += 1;
                if !c.eat(",") {
                    break;
                }
            }
            c.punct(")")?;
        }
        Ok((b, n))
    }

    /// `[base]`, `[base + idx*scale]`, with an optional `+ disp` or `- disp`.
    fn addr(&mut self, c: &mut Cursor<'_>, out: &mut Vec<u32>) -> Result<(), String> {
        c.punct("[")?;
        out.push(self.operand(c, Ty::Ptr)?);
        let (mut idx, mut scale, mut disp) = (Val::NONE.0, 1u32, 0i64);
        loop {
            if c.eat("+") {
                let save = c.at;
                let first = c.next()?;
                if c.eat("*") {
                    c.at = save;
                    idx = self.operand(c, Ty::I64)?;
                    c.punct("*")?;
                    scale = c.num()?;
                } else if let Tok::Num(n) = first {
                    disp = n.parse().map_err(|_| format!("{n} is not a displacement"))?;
                } else {
                    return Err(format!("expected a displacement or an index, found {first:?}"));
                }
            } else if c.eat("-") {
                disp = -c.num::<i64>()?;
            } else {
                break;
            }
        }
        c.punct("]")?;
        let disp = i32::try_from(disp)
            .map_err(|_| format!("the displacement {disp} does not fit in 32 bits"))?;
        out.extend([idx, scale, disp as u32]);
        Ok(())
    }

    fn inst(&mut self, b: Block, toks: &[Tok]) -> Result<(), String> {
        let mut c = Cursor { toks, at: 0 };
        let result = if let (Some(Tok::Val(n)), Some(Tok::Punct("="))) = (toks.first(), toks.get(1))
        {
            c.at = 2;
            Some(n.clone())
        } else {
            None
        };
        let mnemonic = c.word()?;
        let (op, mut ty) = match Op::from_name(&mnemonic) {
            Some(Op::LoadStr | Op::StoreStr) => {
                (Op::from_name(&mnemonic).expect("known"), Ty::Str16)
            }
            Some(Op::LoadBit | Op::Memeq) => (Op::from_name(&mnemonic).expect("known"), Ty::I1),
            Some(Op::Ret) => (Op::Ret, Ty::I64),
            Some(op) => (op, Ty::Void),
            None => {
                let (head, tail) =
                    mnemonic.rsplit_once('.').ok_or(format!("{mnemonic} is not an opcode"))?;
                let op = match head {
                    "load" => Op::Load,
                    "store" => Op::Store,
                    _ => return Err(format!("{mnemonic} is not an opcode")),
                };
                (op, Ty::from_name(tail).ok_or(format!("{tail} is not a type"))?)
            }
        };
        let mut ops = Vec::new();
        match op.form() {
            Form::Un
            | Form::Bin
            | Form::Cmp
            | Form::Wide
            | Form::Cas
            | Form::Atomic
            | Form::Sel => {
                ty = c.ty()?;
                let arity = match op.form() {
                    Form::Un => 1,
                    Form::Sel | Form::Cas => 3,
                    _ => 2,
                };
                for k in 0..arity {
                    if k > 0 {
                        c.punct(",")?;
                    }
                    let t = match (op.form(), k) {
                        (Form::Sel, 0) => Ty::I1,
                        (Form::Cas | Form::Atomic, 0) => Ty::Ptr,
                        _ => ty,
                    };
                    ops.push(self.operand(&mut c, t)?);
                }
            }
            Form::StrMk => {
                ty = Ty::Str16;
                ops.push(self.operand(&mut c, Ty::I64)?);
                c.punct(",")?;
                ops.push(self.operand(&mut c, Ty::I64)?);
            }
            Form::TrapBin | Form::EdgeBin => {
                ty = c.ty()?;
                ops.push(self.operand(&mut c, ty)?);
                c.punct(",")?;
                ops.push(self.operand(&mut c, ty)?);
                c.punct(",")?;
                if op.form() == Form::TrapBin {
                    ops.push(c.err()?);
                } else {
                    ops.push(c.block()?);
                    c.punct(",")?;
                    ops.push(c.block()?);
                }
            }
            Form::TrapUn => {
                ty = c.ty()?;
                ops.push(self.operand(&mut c, ty)?);
                c.punct(",")?;
                ops.push(c.err()?);
            }
            Form::Scale | Form::TrapScale => {
                ty = c.ty()?;
                ops.push(self.operand(&mut c, ty)?);
                c.punct(",")?;
                ops.push(c.num()?);
                if op.form() == Form::TrapScale {
                    c.punct(",")?;
                    ops.push(c.err()?);
                }
            }
            Form::Conv | Form::TrapConv => {
                let from = c.ty()?;
                ops.push(self.operand(&mut c, from)?);
                c.punct("->")?;
                ty = c.ty()?;
                if op.form() == Form::TrapConv {
                    c.punct(",")?;
                    ops.push(c.err()?);
                }
            }
            Form::Load | Form::Prefetch => self.addr(&mut c, &mut ops)?,
            Form::Store => {
                self.addr(&mut c, &mut ops)?;
                c.punct(",")?;
                ops.push(self.operand(&mut c, ty)?);
            }
            Form::LoadBit => {
                c.punct("[")?;
                ops.push(self.operand(&mut c, Ty::Ptr)?);
                c.punct("+")?;
                ops.push(self.operand(&mut c, Ty::I64)?);
                c.punct("]")?;
            }
            Form::Memcpy | Form::Memeq => {
                ops.push(self.operand(&mut c, Ty::Ptr)?);
                c.punct(",")?;
                ops.push(self.operand(&mut c, Ty::Ptr)?);
                c.punct(",")?;
                ops.push(c.num()?);
            }
            Form::Br => {
                ops.push(0);
                let (t, _) = self.target(&mut c, &mut ops)?;
                ops[0] = t;
            }
            Form::Brif => {
                ops.push(self.operand(&mut c, Ty::I1)?);
                c.punct(",")?;
                ops.extend([0, 0]);
                let (t, n) = self.target(&mut c, &mut ops)?;
                ops[1] = t;
                ops[2] = n as u32;
                c.punct(",")?;
                let at = ops.len();
                ops.push(0);
                let (f, _) = self.target(&mut c, &mut ops)?;
                ops[at] = f;
            }
            Form::Switch => {
                ty = c.ty()?;
                ops.push(self.operand(&mut c, ty)?);
                c.punct(",")?;
                ops.extend([0, 0]);
                let (d, n) = self.target(&mut c, &mut ops)?;
                ops[1] = d;
                ops[2] = n as u32;
                c.punct(",")?;
                c.punct("[")?;
                if !matches!(c.peek(), Some(Tok::Punct("]"))) {
                    loop {
                        ops.push(c.num()?);
                        c.punct(":")?;
                        ops.push(c.block()?);
                        if !c.eat(",") {
                            break;
                        }
                    }
                }
                c.punct("]")?;
            }
            Form::Ret => ops.push(self.operand(&mut c, Ty::I64)?),
            Form::Trap => ops.push(c.err()?),
            Form::Rtcall => {
                let name = match c.next()? {
                    Tok::At(n) => n,
                    t => return Err(format!("expected a runtime function, found {t:?}")),
                };
                let id = crate::ir::catalogue::proxy(&name)
                    .ok_or(format!("@{name} is not a runtime function"))?;
                let p = CATALOGUE[id as usize];
                ty = p.ret;
                ops.push(id);
                c.punct("(")?;
                let mut k = 0;
                if !c.eat(")") {
                    loop {
                        let t = p.args.get(k).copied().unwrap_or(Ty::I64);
                        ops.push(self.operand(&mut c, t)?);
                        k += 1;
                        if !c.eat(",") {
                            break;
                        }
                    }
                    c.punct(")")?;
                }
            }
            Form::Vcall => {
                let k = match c.next()? {
                    Tok::At(n) => n
                        .strip_prefix('K')
                        .and_then(|n| n.parse().ok())
                        .ok_or(format!("@{n} is not a kernel"))?,
                    t => return Err(format!("expected a kernel, found {t:?}")),
                };
                ops.push(k);
                c.punct("(")?;
                ops.push(self.operand(&mut c, Ty::I32)?);
                while c.eat(",") {
                    ops.push(self.operand(&mut c, Ty::Ptr)?);
                }
                c.punct(")")?;
            }
            Form::Guard => {
                ops.push(self.operand(&mut c, Ty::I1)?);
                c.punct(",")?;
                match c.next()? {
                    Tok::Guard(g) => ops.push(g),
                    t => return Err(format!("expected a guard, found {t:?}")),
                }
            }
            Form::Poll => ops.push(c.num()?),
            Form::CtrAdd => {
                match c.next()? {
                    Tok::Ctr(k) => ops.push(k),
                    t => return Err(format!("expected a counter, found {t:?}")),
                }
                c.punct(",")?;
                ops.push(self.operand(&mut c, Ty::I64)?);
            }
        }
        let mut flags = 0;
        let mut class = None;
        while let Some(t) = c.peek().cloned() {
            c.at += 1;
            match t {
                Tok::Word(w) if w == "nt" => flags |= NT,
                Tok::Word(w) if w == "a16" => flags |= A16,
                Tok::Word(w) if w == "inv" => flags |= INV,
                Tok::Word(w) if w == "dead" => flags |= DEAD,
                Tok::Word(w) if w == "class" => {
                    c.punct("=")?;
                    let name = c.word()?;
                    class = Some(
                        Class::from_name(&name).ok_or(format!("{name} is not a storage class"))?,
                    );
                }
                t => return Err(format!("unexpected {t:?} at the end of the instruction")),
            }
        }
        c.done()?;
        let rty = op.result(ty);
        let result = match (result, rty) {
            (Some(name), Ty::Void) => {
                return Err(format!("{} has no result to name %{name}", op.name()));
            }
            (None, t) if t != Ty::Void => return Err(format!("{} needs a result", op.name())),
            (Some(name), t) => {
                let v = self.define(&name, t)?;
                if let Some(class) = class {
                    self.f.vals[v.index()].class = class;
                }
                Some(v)
            }
            (None, _) => None,
        };
        self.f.push(b, op, ty, flags, result, &ops, 0);
        Ok(())
    }
}

/// Parses a module.
///
/// # Errors
///
/// The first line that does not parse, and why.
pub fn parse(text: &str) -> Result<Module, ParseError> {
    let lines: Vec<(usize, Vec<Tok>, bool)> = text
        .lines()
        .enumerate()
        .map(|(n, l)| {
            lex(l)
                .map(|t| (n + 1, t, l.starts_with([' ', '\t'])))
                .map_err(|m| ParseError { line: n + 1, message: m })
        })
        .collect::<Result<_, _>>()?;
    let mut m = Module::default();
    let mut i = 0;
    let fail = |line: usize| move |message: String| ParseError { line, message };
    while i < lines.len() {
        let (n, toks, _) = &lines[i];
        let mut c = Cursor { toks, at: 0 };
        match c.peek() {
            None => i += 1,
            Some(Tok::Word(w)) if w == "func" => {
                let end = (i + 1..lines.len())
                    .find(|&j| matches!(lines[j].1.first(), Some(Tok::Word(w)) if w == "func"))
                    .unwrap_or(lines.len());
                let f =
                    func(&lines[i..end]).map_err(|(line, message)| ParseError { line, message })?;
                m.funcs.push(f);
                i = end;
            }
            Some(_) => {
                header(&mut m, &mut c).map_err(fail(*n))?;
                i += 1;
            }
        }
    }
    Ok(m)
}

fn header(m: &mut Module, c: &mut Cursor<'_>) -> Result<(), String> {
    let kw = c.word()?;
    match kw.as_str() {
        "module" => m.name = c.word()?,
        "error" => {
            let _ = c.err()?;
            let kind = c.word()?;
            let kind = ErrorKind::from_name(&kind).ok_or(format!("{kind} is not an error kind"))?;
            let text = c.string()?;
            m.error(kind, &text);
        }
        "guard" => {
            match c.next()? {
                Tok::Guard(_) => {}
                t => return Err(format!("expected a guard, found {t:?}")),
            }
            let fact = c.string()?;
            let key = c.word()?;
            if key != "fallback" {
                return Err(format!("expected fallback=, found {key}"));
            }
            c.punct("=")?;
            let fallback = c.word()?;
            let invariant = matches!(c.peek(), Some(Tok::Word(w)) if w == "inv");
            if invariant {
                c.at += 1;
            }
            m.guard(&fact, &fallback, invariant);
        }
        "counter" => {
            match c.next()? {
                Tok::Ctr(_) => {}
                t => return Err(format!("expected a counter, found {t:?}")),
            }
            let name = c.string()?;
            m.counter(&name);
        }
        "kernel" => {
            let _ = c.next()?;
            let name = c.string()?;
            m.kernel(&name);
        }
        "blob" => {
            let _: u32 = c.num()?;
            let hex = match c.next() {
                Ok(Tok::Num(h) | Tok::Word(h)) => h,
                Ok(t) => return Err(format!("expected hex bytes, found {t:?}")),
                Err(_) => String::new(),
            };
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|k| u8::from_str_radix(hex.get(k..k + 2).unwrap_or("x"), 16))
                .collect::<Result<Vec<u8>, _>>()
                .map_err(|_| "a blob that is not hex".to_owned())?;
            m.blobs.push(bytes);
        }
        _ => return Err(format!("{kw} does not start a module line")),
    }
    c.done()
}

fn func(lines: &[(usize, Vec<Tok>, bool)]) -> Result<Func, (usize, String)> {
    let first = lines[0].0;
    let mut r = FuncReader {
        f: Func { sites: vec![Site { plan: 0, file: "", line: 0 }], ..Func::default() },
        names: HashMap::new(),
        defined: Vec::new(),
    };
    // The function line.
    let mut c = Cursor { toks: &lines[0].1, at: 1 };
    (|| -> Result<(), String> {
        r.f.name = match c.next()? {
            Tok::At(n) => n,
            t => return Err(format!("expected a function name, found {t:?}")),
        };
        while let Some(t) = c.peek().cloned() {
            c.at += 1;
            match t {
                Tok::Word(w) if w == "version" => {
                    c.punct("=")?;
                    r.f.version = c.word()?;
                }
                Tok::Word(w) if w == "plan" => {
                    c.punct("=")?;
                    r.f.plan = match c.next()? {
                        Tok::Ctr(n) => n,
                        t => return Err(format!("expected a plan node, found {t:?}")),
                    };
                }
                Tok::Word(w) if w == "morsel-local" => r.f.morsel_local = true,
                t => return Err(format!("unexpected {t:?} on the function line")),
            }
        }
        Ok(())
    })()
    .map_err(|e| (first, e))?;
    // First pass: the blocks, so branches can name blocks below them.
    let mut declared = std::collections::HashSet::new();
    for (n, toks, indented) in &lines[1..] {
        if !*indented && !toks.is_empty() {
            block_head(&mut r, toks, &mut declared).map_err(|e| (*n, e))?;
        }
    }
    // Second pass: the tables and the instructions.
    let mut cur = None;
    for (n, toks, indented) in &lines[1..] {
        let n = *n;
        if toks.is_empty() {
            continue;
        }
        if !*indented {
            let b = block_id(toks).map_err(|e| (n, e))?;
            cur = Some(Block(b));
            continue;
        }
        let res = match (cur, toks.first()) {
            (None, Some(Tok::Word(w))) if w == "state" => {
                let mut c = Cursor { toks, at: 1 };
                (|| -> Result<(), String> {
                    let offset = c.num()?;
                    let size = c.num()?;
                    let name = c.string()?;
                    r.f.state.push(Field { offset, size, name });
                    c.done()
                })()
            }
            (None, Some(Tok::Word(w))) if w == "valid" => {
                let mut c = Cursor { toks, at: 1 };
                (|| -> Result<(), String> {
                    let v = Val(r.operand(&mut c, Ty::Void)?);
                    c.punct(",")?;
                    let valid = Val(r.operand(&mut c, Ty::I1)?);
                    r.f.validity.push((v, valid));
                    c.done()
                })()
            }
            (Some(b), _) => r.inst(b, toks),
            (None, _) => Err("an instruction before the first block".to_owned()),
        };
        res.map_err(|e| (n, e))?;
    }
    if let Some((name, _)) = r.names.iter().find(|(_, v)| !r.defined[v.index()]) {
        return Err((first, format!("%{name} is used but never defined")));
    }
    if !(0..r.f.blocks.len() as u32).all(|b| declared.contains(&b)) {
        return Err((first, "the blocks are not numbered from b0 without gaps".to_owned()));
    }
    Ok(r.f)
}

fn block_id(toks: &[Tok]) -> Result<u32, String> {
    toks.iter()
        .find_map(|t| match t {
            Tok::Word(w) if w.starts_with('b') && w[1..].parse::<u32>().is_ok() => {
                w[1..].parse().ok()
            }
            _ => None,
        })
        .ok_or_else(|| "a block line with no block".to_owned())
}

fn block_head(
    r: &mut FuncReader,
    toks: &[Tok],
    declared: &mut std::collections::HashSet<u32>,
) -> Result<(), String> {
    let mut c = Cursor { toks, at: 0 };
    let mut data = BlockData::default();
    let id = loop {
        let w = c.word()?;
        match w.as_str() {
            "block" => {}
            "cold" => data.cold = true,
            "bounded" => data.bounded = true,
            "batch" => data.batch = true,
            "loop" => {
                data.is_loop = true;
                c.punct("(")?;
                data.depth = c.num()?;
                c.punct(")")?;
            }
            _ => match w.strip_prefix('b').and_then(|n| n.parse::<u32>().ok()) {
                Some(id) => break id,
                None => return Err(format!("{w} is not a block")),
            },
        }
    };
    if c.eat("(") {
        loop {
            let ty = c.ty()?;
            let name = match c.next()? {
                Tok::Val(n) => n,
                t => return Err(format!("expected a parameter, found {t:?}")),
            };
            let v = r.define(&name, ty)?;
            if matches!(c.peek(), Some(Tok::Word(w)) if w == "class") {
                c.at += 1;
                c.punct("=")?;
                let class = c.word()?;
                r.f.vals[v.index()].class =
                    Class::from_name(&class).ok_or(format!("{class} is not a storage class"))?;
            }
            data.params.push(v);
            if !c.eat(",") {
                break;
            }
        }
        c.punct(")")?;
    }
    c.punct(":")?;
    c.done()?;
    if !declared.insert(id) {
        return Err(format!("b{id} is declared twice"));
    }
    let at = id as usize;
    if r.f.blocks.len() <= at {
        r.f.blocks.resize(at + 1, BlockData::default());
    }
    r.f.blocks[at] = data;
    Ok(())
}

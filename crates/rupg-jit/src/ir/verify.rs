// Lifted from rudb crates/rudb-qc-ir/src/verify.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the crate are under `crate::ir`.

//! The verifier of section 6.10 of `spec/compiler/06-qir.md`.
//!
//! It runs on every module in debug builds, in the differential harness, and after the parser in
//! every text test. Rules V9 to V12 are the engine's semantic invariants: each turns what would
//! look like a backend miscompiling into "the generator produced an illegal module", and each has
//! a module under `tests/illegal/` that must be rejected. V13 expects dead code elimination to
//! have run.

use std::fmt;

use crate::ir::cfg::Cfg;
use crate::ir::func::{DEAD, Inst};
use crate::ir::{Block, CATALOGUE, Class, Form, Func, Module, Op, Ty, Val};

/// One broken rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError {
    /// The rule, `V1` to `V13`.
    pub rule: &'static str,
    /// The function.
    pub func: String,
    /// The block.
    pub block: u32,
    /// The instruction's position in its block, when the error is about one.
    pub inst: Option<u32>,
    /// What is wrong.
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inst {
            Some(i) => {
                write!(f, "{}: @{} b{} #{}: {}", self.rule, self.func, self.block, i, self.message)
            }
            None => write!(f, "{}: @{} b{}: {}", self.rule, self.func, self.block, self.message),
        }
    }
}

/// Checks every rule on every function of a module.
///
/// # Errors
///
/// Every broken rule found, in function and block order.
pub fn verify(m: &Module) -> Result<(), Vec<VerifyError>> {
    let mut errors = Vec::new();
    for f in &m.funcs {
        Checker::new(m, f, &mut errors).run();
    }
    if errors.is_empty() { Ok(()) } else { Err(errors) }
}

fn polls(insts: &[Inst<'_>]) -> bool {
    insts.iter().any(|i| i.op == Op::Poll && !i.dead())
}

/// Where a value is defined: its block and its position, with parameters at 0 and instruction
/// `k` at `k + 1`.
#[derive(Clone, Copy)]
struct Def {
    block: Block,
    pos: u32,
    dead: bool,
}

/// Every instruction of a function, decoded once, with where each block's start.
#[derive(Default)]
struct Code<'a> {
    insts: Vec<Inst<'a>>,
    starts: Vec<usize>,
}

impl<'a> Code<'a> {
    fn of(&self, b: Block) -> &[Inst<'a>] {
        &self.insts[self.starts[b.index()]..self.starts[b.index() + 1]]
    }
}

struct Checker<'a> {
    m: &'a Module,
    f: &'a Func,
    cfg: Cfg,
    defs: Vec<Option<Def>>,
    code: Code<'a>,
    errors: &'a mut Vec<VerifyError>,
    block: Block,
    pos: Option<u32>,
}

impl<'a> Checker<'a> {
    fn new(m: &'a Module, f: &'a Func, errors: &'a mut Vec<VerifyError>) -> Checker<'a> {
        let mut defs = vec![None; f.vals.len()];
        let mut code = Code { insts: Vec::new(), starts: Vec::with_capacity(f.blocks.len() + 1) };
        for (b, data) in f.blocks.iter().enumerate() {
            let block = Block(b as u32);
            for p in &data.params {
                if let Some(d) = defs.get_mut(p.index()) {
                    *d = Some(Def { block, pos: 0, dead: false });
                }
            }
            code.starts.push(code.insts.len());
            for (k, i) in f.insts(block).enumerate() {
                if let Some(r) = i.result
                    && let Some(d) = defs.get_mut(r.index())
                {
                    *d = Some(Def { block, pos: k as u32 + 1, dead: i.dead() });
                }
                code.insts.push(i);
            }
        }
        code.starts.push(code.insts.len());
        let cfg = Cfg::with(f, |b| code.of(b).last().copied());
        Checker { m, f, cfg, defs, code, errors, block: Block(0), pos: None }
    }

    fn fail(&mut self, rule: &'static str, message: String) {
        self.errors.push(VerifyError {
            rule,
            func: self.f.name.clone(),
            block: self.block.0,
            inst: self.pos,
            message,
        });
    }

    fn ty(&self, v: Val) -> Ty {
        if v.is_const() {
            self.f.consts.get(v.const_index()).map_or(Ty::Void, |c| c.ty)
        } else {
            self.f.vals.get(v.index()).map_or(Ty::Void, |i| i.ty)
        }
    }

    fn name(&self, v: Val) -> String {
        match self.f.constant(v) {
            Some(c) => crate::ir::print::konst(c.ty, c.bits),
            None => format!(
                "%{}",
                self.f.vals.get(v.index()).and_then(|i| i.name.as_deref()).unwrap_or("?")
            ),
        }
    }

    fn want(&mut self, v: Val, ty: Ty, what: impl fmt::Display) {
        let got = self.ty(v);
        if got != ty {
            let name = self.name(v);
            self.fail("V2", format!("{what} {name} is {}, it should be {}", got.name(), ty.name()));
        }
    }

    fn want_int(&mut self, v: Val, what: impl fmt::Display) {
        let got = self.ty(v);
        if !got.is_int() {
            let name = self.name(v);
            self.fail("V2", format!("{what} {name} is {}, it should be an integer", got.name()));
        }
    }

    fn run(&mut self) {
        let code = std::mem::take(&mut self.code);
        self.entry();
        for b in 0..self.f.blocks.len() {
            self.block = Block(b as u32);
            self.pos = None;
            let insts = code.of(self.block);
            self.terminators(insts);
            if !self.cfg.reachable(self.block) {
                continue;
            }
            for (k, i) in insts.iter().enumerate() {
                if i.dead() {
                    continue;
                }
                self.pos = Some(k as u32);
                self.operands_dominate(i, k as u32 + 1);
                self.types(i);
                self.tables(i);
                self.state(i);
                self.strings(i);
            }
        }
        self.pos = None;
        self.loops(&code);
        self.guards_after_effects(&code);
        self.validity(&code);
        self.unused(&code);
    }

    /// V5: the entry block has the ABI parameters.
    fn entry(&mut self) {
        self.block = Block(0);
        let ok = self.f.blocks.first().is_some_and(|b| {
            b.params.len() == 2
                && b.params
                    .iter()
                    .all(|p| self.f.vals.get(p.index()).is_some_and(|i| i.ty == Ty::Ptr))
        });
        if !ok {
            self.fail(
                "V5",
                "the entry block must have the parameters (ptr %st, ptr %m)".to_owned(),
            );
        }
        if self.f.blocks.first().is_some_and(|b| b.is_loop) {
            self.fail("V4", "the entry block cannot be a loop header".to_owned());
        }
    }

    /// V3: one terminator, last.
    fn terminators(&mut self, insts: &[Inst<'a>]) {
        match insts.last() {
            None => self.fail("V3", "the block is empty".to_owned()),
            Some(last) if !last.op.is_terminator() || last.dead() => {
                self.fail(
                    "V3",
                    format!("the block ends in {}, which is not a terminator", last.op.name()),
                );
            }
            Some(_) => {}
        }
        for (k, i) in insts.iter().enumerate().take(insts.len().saturating_sub(1)) {
            if i.op.is_terminator() {
                self.pos = Some(k as u32);
                self.fail("V3", format!("{} in the middle of the block", i.op.name()));
            }
        }
        self.pos = None;
    }

    /// V1: every operand dominates its use.
    fn operands_dominate(&mut self, i: &Inst<'_>, pos: u32) {
        let mut bad = Vec::new();
        i.uses(|v| {
            if v.is_const() || v == Val::NONE {
                if v.is_const() && v.const_index() >= self.f.consts.len() {
                    bad.push((v, "is not in the constant pool"));
                }
                return;
            }
            match self.defs.get(v.index()).copied().flatten() {
                None => bad.push((v, "is never defined")),
                Some(d) if d.dead => bad.push((v, "was removed by dead code elimination")),
                Some(d) if d.block == self.block => {
                    if d.pos >= pos {
                        bad.push((v, "is used before its definition"));
                    }
                }
                Some(d) => {
                    if !self.cfg.dominates(d.block, self.block) {
                        bad.push((v, "is defined in a block that does not dominate this one"));
                    }
                }
            }
        });
        for (v, why) in bad {
            let name = self.name(v);
            self.fail("V1", format!("{name} {why}"));
        }
    }

    fn args(&mut self, target: Block, args: &[u32]) {
        let Some(data) = self.f.blocks.get(target.index()) else {
            self.fail("V2", format!("b{} does not exist", target.0));
            return;
        };
        let params = &data.params;
        if params.len() != args.len() {
            let n = params.len();
            self.fail("V2", format!("b{} takes {n} arguments and gets {}", target.0, args.len()));
            return;
        }
        for (p, a) in params.iter().zip(args) {
            let ty = self.ty(*p);
            self.want(Val(*a), ty, format_args!("the argument to b{}", target.0));
        }
    }

    /// V2 and V8: operand and result types, and the constant limits.
    fn types(&mut self, i: &Inst<'_>) {
        let o = i.ops;
        let v = |k: usize| Val(o[k]);
        let ty = i.ty;
        let name = i.op.name();
        let float_op = matches!(
            i.op,
            Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::FminTot
                | Op::FmaxTot
                | Op::Fneg
                | Op::Fabs
                | Op::Fsqrt
        ) || matches!(i.op, Op::FcmpEq | Op::FcmpLt | Op::FcmpLe);
        let str_op = matches!(i.op, Op::StrLen | Op::StrW0 | Op::StrW1 | Op::StrPtr | Op::StrInl);
        match i.op.form() {
            Form::Un | Form::Bin | Form::Cmp | Form::Wide => {
                if float_op && !ty.is_float() {
                    self.fail("V2", format!("{name} needs a float type, not {}", ty.name()));
                } else if str_op && ty != Ty::Str16 {
                    self.fail("V2", format!("{name} needs a str16, not {}", ty.name()));
                } else if !float_op
                    && !str_op
                    && !ty.is_int()
                    && !(ty == Ty::Ptr && i.op.form() == Form::Cmp)
                {
                    self.fail("V2", format!("{name} needs an integer type, not {}", ty.name()));
                }
                if i.op == Op::Crc32c && ty != Ty::I64 {
                    self.fail("V2", "crc32c works on i64".to_owned());
                }
                if i.op.form() == Form::Wide && ty.bits() > 64 {
                    self.fail("V2", format!("{name} widens at most an i64"));
                }
                for k in 0..o.len() {
                    self.want(v(k), ty, format_args!("operand {k} of {name}"));
                }
            }
            Form::Sel => {
                self.want(v(0), Ty::I1, "the condition of select");
                self.want(v(1), ty, "operand 1 of select");
                self.want(v(2), ty, "operand 2 of select");
            }
            Form::TrapBin | Form::EdgeBin => {
                if !ty.is_int() || ty == Ty::I1 {
                    self.fail("V2", format!("{name} needs an integer type, not {}", ty.name()));
                }
                self.want(v(0), ty, format_args!("operand 0 of {name}"));
                self.want(v(1), ty, format_args!("operand 1 of {name}"));
                if i.op.form() == Form::EdgeBin {
                    let (ok, ovf) = (Block(o[2]), Block(o[3]));
                    let okp: Vec<Ty> = self
                        .f
                        .blocks
                        .get(ok.index())
                        .map(|b| b.params.iter().map(|p| self.ty(*p)).collect())
                        .unwrap_or_default();
                    if okp != [ty] {
                        self.fail(
                            "V2",
                            format!("the ok block b{} of {name} must take one {}", ok.0, ty.name()),
                        );
                    }
                    if self.f.blocks.get(ovf.index()).is_none_or(|b| !b.params.is_empty()) {
                        self.fail(
                            "V2",
                            format!("the overflow block b{} of {name} must take nothing", ovf.0),
                        );
                    }
                }
            }
            Form::TrapUn | Form::Scale | Form::TrapScale => {
                if !ty.is_int() || ty == Ty::I1 {
                    self.fail("V2", format!("{name} needs an integer type, not {}", ty.name()));
                }
                self.want(v(0), ty, format_args!("the operand of {name}"));
                if matches!(i.op.form(), Form::Scale | Form::TrapScale) && o[1] > 38 {
                    self.fail("V8", format!("{name} by 10^{} is past 38", o[1]));
                }
            }
            Form::Conv | Form::TrapConv => self.conversion(i.op, self.ty(v(0)), ty),
            Form::Load | Form::Prefetch | Form::Store => {
                self.want(v(0), Ty::Ptr, "the base");
                if o[1] != Val::NONE.0 {
                    self.want_int(v(1), "the index");
                }
                if !matches!(o[2], 1 | 2 | 4 | 8 | 16) {
                    self.fail("V2", format!("the scale {} is not 1, 2, 4, 8 or 16", o[2]));
                }
                if i.op.form() == Form::Store {
                    self.want(v(4), ty, "the stored value");
                }
                let str_mem = matches!(i.op, Op::LoadStr | Op::StoreStr);
                if i.op.form() != Form::Prefetch && (ty == Ty::Void || str_mem != (ty == Ty::Str16))
                {
                    self.fail("V2", format!("{name} cannot move a {}", ty.name()));
                }
            }
            Form::LoadBit => {
                self.want(v(0), Ty::Ptr, "the bitmap");
                self.want_int(v(1), "the bit index");
            }
            Form::Memcpy | Form::Memeq => {
                self.want(v(0), Ty::Ptr, format_args!("operand 0 of {name}"));
                self.want(v(1), Ty::Ptr, format_args!("operand 1 of {name}"));
                if o[2] == 0 || o[2] > 64 {
                    self.fail("V8", format!("{name} of {} bytes is outside 1 to 64", o[2]));
                }
            }
            Form::Cas | Form::Atomic => {
                if !ty.is_int() || ty == Ty::I1 {
                    self.fail("V2", format!("{name} needs an integer type, not {}", ty.name()));
                }
                self.want(v(0), Ty::Ptr, "the address");
                for k in 1..o.len() {
                    self.want(v(k), ty, format_args!("operand {k} of {name}"));
                }
            }
            Form::StrMk => {
                self.want(v(0), Ty::I64, "the first word");
                self.want(v(1), Ty::I64, "the second word");
            }
            Form::Br => self.args(Block(o[0]), &o[1..]),
            Form::Brif => {
                self.want(v(0), Ty::I1, "the condition");
                let n = o[2] as usize;
                self.args(Block(o[1]), &o[3..3 + n]);
                self.args(Block(o[3 + n]), &o[4 + n..]);
            }
            Form::Switch => {
                if !ty.is_int() {
                    self.fail("V2", format!("switch needs an integer type, not {}", ty.name()));
                }
                self.want(v(0), ty, "the switch value");
                let n = o[2] as usize;
                self.args(Block(o[1]), &o[3..3 + n]);
                for pair in o[3 + n..].as_chunks::<2>().0 {
                    self.args(Block(pair[1]), &[]);
                }
            }
            Form::Ret => self.want(v(0), Ty::I64, "the status"),
            Form::Trap | Form::Poll => {}
            Form::Rtcall => {
                let Some(p) = CATALOGUE.get(o[0] as usize) else { return };
                if ty != p.ret {
                    self.fail(
                        "V2",
                        format!("@{} returns {}, not {}", p.name, p.ret.name(), ty.name()),
                    );
                }
                if o.len() - 1 != p.args.len() {
                    self.fail(
                        "V2",
                        format!(
                            "@{} takes {} arguments and gets {}",
                            p.name,
                            p.args.len(),
                            o.len() - 1
                        ),
                    );
                    return;
                }
                for (k, want) in p.args.iter().enumerate() {
                    self.want(v(k + 1), *want, format_args!("argument {k} of @{}", p.name));
                }
            }
            Form::Vcall => {
                self.want_int(v(1), "the element count");
                for k in 2..o.len() {
                    self.want(v(k), Ty::Ptr, "a kernel buffer");
                }
            }
            Form::Guard => self.want(v(0), Ty::I1, "the guard condition"),
            Form::CtrAdd => self.want_int(v(1), "the counter increment"),
        }
    }

    fn conversion(&mut self, op: Op, from: Ty, to: Ty) {
        let ok = match op {
            Op::Sext | Op::Zext => from.is_int() && to.is_int() && from.bits() < to.bits(),
            Op::Trunc => from.is_int() && to.is_int() && from.bits() > to.bits(),
            Op::Sitof | Op::Uitof => from.is_int() && to.is_float(),
            Op::FtosiT => from.is_float() && to.is_int() && to != Ty::I1,
            Op::Fext => from == Ty::F32 && to == Ty::F64,
            Op::Ftrunc => from == Ty::F64 && to == Ty::F32,
            Op::Bitcast => from.bits() == to.bits() && from != Ty::Str16 && to != Ty::Str16,
            _ => false,
        };
        if !ok {
            self.fail(
                "V2",
                format!("{} cannot convert {} to {}", op.name(), from.name(), to.name()),
            );
        }
    }

    /// V7: every table reference resolves.
    fn tables(&mut self, i: &Inst<'_>) {
        let o = i.ops;
        let (what, id, len) = match i.op.form() {
            Form::TrapBin => ("error site", o[2], self.m.errors.len()),
            Form::TrapUn | Form::TrapConv | Form::Trap => {
                ("error site", o[o.len() - 1], self.m.errors.len())
            }
            Form::TrapScale => ("error site", o[2], self.m.errors.len()),
            Form::Guard => ("guard", o[1], self.m.guards.len()),
            Form::CtrAdd => ("counter", o[0], self.m.counters.len()),
            Form::Rtcall => ("runtime function", o[0], CATALOGUE.len()),
            Form::Vcall => ("kernel", o[0], self.m.kernels.len()),
            _ => return,
        };
        if id as usize >= len {
            self.fail("V7", format!("{what} {id} is not in the module's table"));
        }
    }

    /// V6: accesses at a constant offset from `%st` fall inside a declared field.
    fn state(&mut self, i: &Inst<'_>) {
        if self.f.state.is_empty() || !matches!(i.op.form(), Form::Load | Form::Store) {
            return;
        }
        let st = self.f.blocks[0].params.first().copied();
        if Some(Val(i.ops[0])) != st || i.ops[1] != Val::NONE.0 {
            return;
        }
        let at = i64::from(i.ops[3] as i32);
        let width = i64::from(i.ty.bytes());
        let inside = self.f.state.iter().any(|fl| {
            at >= i64::from(fl.offset) && at + width <= i64::from(fl.offset) + i64::from(fl.size)
        });
        if !inside {
            self.fail(
                "V6",
                format!(
                    "{} of {width} bytes at %st + {at} is outside every state field",
                    i.op.name()
                ),
            );
        }
    }

    /// V10: a transient string is never stored, and never passed where a longer lived one is
    /// expected.
    fn strings(&mut self, i: &Inst<'_>) {
        let class = |v: Val| {
            if v.is_const() {
                Class::Persistent
            } else {
                self.f.vals.get(v.index()).map_or(Class::Persistent, |x| x.class)
            }
        };
        if i.op == Op::StoreStr && class(Val(i.ops[4])) == Class::Transient {
            let name = self.name(Val(i.ops[4]));
            self.fail(
                "V10",
                format!("{name} is transient and is stored; promote it with @str_promote first"),
            );
        }
        let mut bad = Vec::new();
        i.succs(|b, args| {
            let params = &self.f.blocks[b.index()].params;
            for (p, a) in params.iter().zip(args) {
                if self.ty(*p) == Ty::Str16
                    && class(Val(*a)) == Class::Transient
                    && class(*p) != Class::Transient
                {
                    bad.push((Val(*a), b));
                }
            }
        });
        for (a, b) in bad {
            let name = self.name(a);
            self.fail(
                "V10",
                format!("{name} is transient and b{} takes it as a longer lived string", b.0),
            );
        }
    }

    /// V4 and V11: back edges go to loop headers, depths nest, and unbounded loops poll.
    fn loops(&mut self, code: &Code<'_>) {
        let n = self.f.blocks.len();
        let mut has_back_edge = vec![false; n];
        let mut body_of: Vec<Vec<Block>> = vec![Vec::new(); n];
        for &b in &self.cfg.rpo.clone() {
            for &s in &self.cfg.succs[b.index()].clone() {
                self.block = b;
                if self.cfg.is_back_edge(b, s) {
                    has_back_edge[s.index()] = true;
                    let header = &self.f.blocks[s.index()];
                    if !header.is_loop {
                        self.fail(
                            "V4",
                            format!("the back edge to b{} goes to a block not flagged loop", s.0),
                        );
                        continue;
                    }
                    if !header.batch && !header.bounded && !polls(code.of(s)) && !polls(code.of(b))
                    {
                        self.fail(
                            "V11",
                            format!(
                                "the back edge to b{} has no poll in b{} or in b{}",
                                s.0, s.0, b.0
                            ),
                        );
                    }
                    // The natural loop of this edge, for the depth check.
                    let mut stack = vec![b];
                    let body = &mut body_of[s.index()];
                    while let Some(x) = stack.pop() {
                        if x == s || body.contains(&x) {
                            continue;
                        }
                        body.push(x);
                        stack.extend(self.cfg.preds[x.index()].iter().copied());
                    }
                } else if self.cfg.order[s.index()] <= self.cfg.order[b.index()] {
                    self.fail("V4", format!("the edge to b{} is a retreating edge that is not a back edge, so the graph is not reducible", s.0));
                }
            }
        }
        for h in 0..n {
            self.block = Block(h as u32);
            let data = &self.f.blocks[h];
            if data.is_loop && !has_back_edge[h] && self.cfg.reachable(Block(h as u32)) {
                self.fail(
                    "V4",
                    "the block is flagged loop and nothing branches back to it".to_owned(),
                );
            }
            for inner in body_of[h].clone() {
                let d = &self.f.blocks[inner.index()];
                if d.is_loop && d.depth <= self.f.blocks[h].depth {
                    self.fail(
                        "V4",
                        format!(
                            "b{} is nested in this loop and its depth {} is not deeper than {}",
                            inner.0, d.depth, self.f.blocks[h].depth
                        ),
                    );
                }
            }
        }
    }

    fn effect(i: &Inst<'_>) -> bool {
        match i.op {
            Op::Store | Op::StoreStr | Op::Memcpy | Op::Cas | Op::AtomicAdd => true,
            Op::Rtcall => CATALOGUE.get(i.ops[0] as usize).is_some_and(|p| p.effect),
            _ => false,
        }
    }

    /// V9: no guard is reachable from an effect.
    fn guards_after_effects(&mut self, code: &Code<'_>) {
        if self.f.morsel_local {
            return;
        }
        let n = self.f.blocks.len();
        let mut entered = vec![false; n];
        let mut work: Vec<Block> = self.cfg.rpo.clone();
        let mut queued = vec![true; n];
        let exits = |b: Block, entered: bool| {
            entered || code.of(b).iter().any(|i| !i.dead() && Self::effect(i))
        };
        while let Some(b) = work.pop() {
            queued[b.index()] = false;
            if exits(b, entered[b.index()]) {
                for &s in &self.cfg.succs[b.index()] {
                    if !entered[s.index()] {
                        entered[s.index()] = true;
                        if !queued[s.index()] {
                            queued[s.index()] = true;
                            work.push(s);
                        }
                    }
                }
            }
        }
        for &b in &self.cfg.rpo.clone() {
            self.block = b;
            let mut after = entered[b.index()];
            for (k, i) in code.of(b).iter().enumerate() {
                if i.dead() {
                    continue;
                }
                if i.op == Op::Guard && after {
                    self.pos = Some(k as u32);
                    self.fail("V9", "a guard is reachable from an effect earlier in the invocation, so rerunning the morsel would repeat it".to_owned());
                }
                after |= Self::effect(i);
            }
            self.pos = None;
        }
    }

    /// V12: a trapping instruction on a possibly invalid operand is guarded by its validity.
    fn validity(&mut self, code: &Code<'a>) {
        let f = self.f;
        if f.validity.iter().all(|(_, valid)| valid.is_const()) {
            return;
        }
        let mut traps: Vec<(Block, u32, Inst<'a>)> = Vec::new();
        for &b in &self.cfg.rpo {
            for (k, &i) in code.of(b).iter().enumerate() {
                let form = i.op.form();
                if !i.dead()
                    && matches!(
                        form,
                        Form::TrapBin
                            | Form::TrapUn
                            | Form::TrapScale
                            | Form::TrapConv
                            | Form::EdgeBin
                    )
                {
                    traps.push((b, k as u32, i));
                }
            }
        }
        for &(v, valid) in &f.validity {
            if valid.is_const() {
                continue;
            }
            for (b, k, i) in &traps {
                let mut reads = false;
                i.uses(|u| reads |= u == v);
                if reads && !self.guarded_by(code, *b, valid) {
                    self.block = *b;
                    self.pos = Some(*k);
                    let (vn, valn) = (self.name(v), self.name(valid));
                    self.fail("V12", format!("{} traps on {vn}, which may be invalid, and no branch on {valn} guards it", i.op.name()));
                }
            }
        }
        self.pos = None;
    }

    /// Whether every path to `b` took the true edge of a `brif` on `valid`.
    fn guarded_by(&self, code: &Code<'_>, b: Block, valid: Val) -> bool {
        let mut x = b;
        loop {
            if x == Block(0) {
                return false;
            }
            let d = self.cfg.idom[x.index()];
            if let Some(t) = code.of(d).last()
                && t.op == Op::Brif
                && Val(t.ops[0]) == valid
            {
                let n = t.ops[2] as usize;
                let (yes, no) = (Block(t.ops[1]), Block(t.ops[3 + n]));
                // The true side must be entered only from this branch, and must dominate `b`.
                if yes != no && self.cfg.preds[yes.index()].len() == 1 && self.cfg.dominates(yes, b)
                {
                    return true;
                }
            }
            x = d;
        }
    }

    /// V13: nothing computes a value nobody reads, unless it has an effect or can trap.
    fn unused(&mut self, code: &Code<'_>) {
        let mut uses = vec![0u32; self.f.vals.len()];
        for b in 0..self.f.blocks.len() {
            for i in code.of(Block(b as u32)) {
                if !i.dead() {
                    i.uses(|v| {
                        if !v.is_const() && v != Val::NONE && v.index() < uses.len() {
                            uses[v.index()] += 1;
                        }
                    });
                }
            }
        }
        for b in 0..self.f.blocks.len() {
            self.block = Block(b as u32);
            for (k, i) in code.of(Block(b as u32)).iter().enumerate() {
                let Some(r) = i.result else { continue };
                if i.dead() || i.op.has_effect() || i.op.traps() || i.flags & DEAD != 0 {
                    continue;
                }
                if uses.get(r.index()) == Some(&0) {
                    self.pos = Some(k as u32);
                    let name = self.name(r);
                    self.fail("V13", format!("{name} is never used; run dead code elimination"));
                }
            }
        }
        self.pos = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Builder, ErrorKind};

    fn module(f: Func) -> Module {
        let mut m = Module::new("t");
        m.error(ErrorKind::Overflow, "INT32 (a + b)");
        m.guard("F1", "t.generic", true);
        m.funcs.push(f);
        m
    }

    fn rules(m: &Module) -> Vec<&'static str> {
        verify(m).err().unwrap_or_default().into_iter().map(|e| e.rule).collect()
    }

    #[test]
    fn a_small_loop_verifies() {
        let mut b = Builder::new("f", "generic", 0);
        let m = b.m();
        let n = b.load(Ty::I32, m, Val::NONE, 1, 8, crate::ir::func::INV);
        let zero = b.int(Ty::I32, 0);
        let head = b.block(&[(Ty::I32, "i")]);
        let exit = b.block(&[]);
        b.set_loop(head, 1);
        b.set_batch(head);
        b.br(head, &[zero]);
        b.switch_to(head);
        let i = b.param(head, 0);
        let one = b.int(Ty::I32, 1);
        let next = b.bin(Op::Add, i, one);
        let done = b.bin(Op::IcmpUge, next, n);
        b.brif(done, exit, &[], head, &[next]);
        b.switch_to(exit);
        let ok = b.int(Ty::I64, 0);
        b.ret(ok);
        let m = module(b.finish());
        assert_eq!(verify(&m), Ok(()));
    }

    #[test]
    fn an_unbounded_loop_without_a_poll_is_rejected() {
        let mut b = Builder::new("f", "generic", 0);
        let head = b.block(&[]);
        b.set_loop(head, 1);
        b.br(head, &[]);
        b.switch_to(head);
        b.br(head, &[]);
        assert_eq!(rules(&module(b.finish())), ["V11"]);
    }

    #[test]
    fn a_back_edge_to_an_unflagged_block_is_rejected() {
        let mut b = Builder::new("f", "generic", 0);
        let head = b.block(&[]);
        b.br(head, &[]);
        b.switch_to(head);
        b.poll(1024);
        b.br(head, &[]);
        assert_eq!(rules(&module(b.finish())), ["V4"]);
    }

    #[test]
    fn a_use_that_is_not_dominated_is_rejected() {
        let mut b = Builder::new("f", "generic", 0).literal();
        let st = b.st();
        let c = b.load(Ty::I1, st, Val::NONE, 1, 0, 0);
        let (l, r, j) = (b.block(&[]), b.block(&[]), b.block(&[]));
        b.brif(c, l, &[], r, &[]);
        b.switch_to(l);
        let x = b.load(Ty::I64, st, Val::NONE, 1, 8, 0);
        b.br(j, &[]);
        b.switch_to(r);
        b.br(j, &[]);
        b.switch_to(j);
        b.ret(x);
        assert_eq!(rules(&module(b.finish())), ["V1"]);
    }
}

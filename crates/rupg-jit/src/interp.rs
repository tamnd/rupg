// Lifted from rudb crates/rudb-qc-interp/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit that reads QIR from `crate::ir`.

//! `interp`, the query compiler's first tier: it runs a QIR function without generating machine
//! code, per `spec/compiler/08-backends.md`.
//!
//! Lowering turns each function into a flat list of instructions over a register file, one
//! register per QIR value and one per constant, with branch targets resolved to positions and
//! block arguments turned into lists of moves. The dispatch loop then walks that list. Arithmetic
//! goes through [`crate::ir::eval`], the same code the builder folds with, so the interpreter and
//! the folder cannot disagree about a result.
//!
//! Memory is the one place this crate is unsafe: a load or a store goes through the address the
//! function computed. The generator is what makes those addresses valid, and the verifier is
//! what checks the generator; this crate trusts both, as every backend does.

#![deny(unsafe_code)]

use crate::ir::eval::{self, Fault};
use crate::ir::func::DEAD;
use crate::ir::{Form, Func, Module, Op, Ty, Val, status};

/// What a running function reaches outside itself: runtime functions, first engine kernels,
/// counters and the cancel flag.
pub trait Runtime {
    /// Calls runtime function `proxy`. An `Err` is a status the function returns at once.
    ///
    /// # Errors
    ///
    /// The status to return when the call fails.
    fn rtcall(&mut self, proxy: u32, args: &[u128]) -> Result<u128, u64>;

    /// [`rtcall`](Runtime::rtcall) with the value written to `out`, and the status returned, 0
    /// when it worked. Native code calls this, and a runtime can write the value of a call made
    /// once a row straight into the caller's slot, which spares reading it back from a `Result`
    /// with a wider load than the stores that just wrote it.
    fn rtcall_into(&mut self, proxy: u32, args: &[u128], out: &mut u128) -> u64 {
        match self.rtcall(proxy, args) {
            Ok(v) => {
                *out = v;
                0
            }
            Err(s) => s,
        }
    }

    /// Runs first engine kernel `kernel` over `n` elements of the buffers.
    ///
    /// # Errors
    ///
    /// The status to return when the kernel fails.
    fn vcall(&mut self, kernel: u32, n: u64, buffers: &[u128]) -> Result<(), u64>;

    /// Adds `v` to counter `k`.
    fn count(&mut self, k: u32, v: u64);

    /// Whether the query has been cancelled.
    fn cancelled(&self) -> bool;
}

/// A range in one of the side tables.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u32,
    len: u32,
}

const NO_REG: u32 = u32::MAX;

#[derive(Clone, Copy, Debug)]
enum Ins {
    Un { op: Op, ty: Ty, to: Ty, d: u32, a: u32 },
    Bin { op: Op, ty: Ty, d: u32, a: u32, b: u32 },
    AddI64 { d: u32, a: u32, b: u32 },
    Sel { d: u32, c: u32, a: u32, b: u32 },
    TrapBin { op: Op, ty: Ty, d: u32, a: u32, b: u32, err: u32 },
    TrapUn { op: Op, ty: Ty, to: Ty, d: u32, a: u32, err: u32 },
    Scale { op: Op, ty: Ty, d: u32, a: u32, k: u32, err: u32 },
    Edge { op: Op, ty: Ty, a: u32, b: u32, ok: u32, okreg: u32, ovf: u32 },
    Load { ty: Ty, d: u32, base: u32, idx: u32, scale: u32, disp: i32 },
    Store { ty: Ty, base: u32, idx: u32, scale: u32, disp: i32, v: u32 },
    LoadBit { d: u32, base: u32, idx: u32 },
    Memcpy { dst: u32, src: u32, n: u32 },
    Memeq { d: u32, a: u32, b: u32, n: u32 },
    Cas { ty: Ty, d: u32, addr: u32, old: u32, new: u32 },
    Atomic { ty: Ty, d: u32, addr: u32, v: u32 },
    Jump { pc: u32, moves: Span },
    Brif { c: u32, t: u32, tm: Span, f: u32, fm: Span },
    Switch { x: u32, default: u32, dm: Span, cases: Span },
    Ret { a: u32 },
    Trap { err: u32 },
    Rtcall { proxy: u32, d: u32, args: Span },
    Vcall { kernel: u32, n: u32, bufs: Span },
    Guard { c: u32, g: u32 },
    Poll { n: u32 },
    CtrAdd { k: u32, v: u32 },
    Nop,
}

/// One lowered function.
#[derive(Clone, Debug)]
struct Code {
    name: String,
    ins: Vec<Ins>,
    /// The register file a call starts from: zero for values, the pool for constants.
    init: Vec<u128>,
    /// `(dst, src)` register pairs for block arguments.
    moves: Vec<(u32, u32)>,
    /// Register lists for call arguments.
    regs: Vec<u32>,
    /// `(key, pc)` for switch cases.
    cases: Vec<(u128, u32)>,
    /// The registers of the entry block's parameters.
    st: u32,
    m: u32,
}

/// A module lowered for the interpreter.
#[derive(Clone, Debug)]
pub struct Program {
    funcs: Vec<Code>,
}

impl Program {
    /// Lowers every function of a module.
    #[must_use]
    pub fn new(m: &Module) -> Program {
        Program { funcs: m.funcs.iter().map(lower).collect() }
    }

    /// The index of the function with this name.
    #[must_use]
    pub fn func(&self, name: &str) -> Option<usize> {
        self.funcs.iter().position(|c| c.name == name)
    }

    /// How many instructions the lowered form of function `f` has, for tests and `EXPLAIN`.
    #[must_use]
    pub fn len(&self, f: usize) -> usize {
        self.funcs[f].ins.len()
    }

    /// Whether the program has no functions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.funcs.is_empty()
    }

    /// Runs function `f` on a state and a morsel, and returns its status.
    pub fn call(&self, f: usize, st: *mut u8, m: *const u8, rt: &mut dyn Runtime) -> u64 {
        let code = &self.funcs[f];
        let mut r = code.init.clone();
        r[code.st as usize] = st.expose_provenance() as u128;
        r[code.m as usize] = m.expose_provenance() as u128;
        run(code, &mut r, rt)
    }
}

fn reg(nvals: usize, v: Val) -> u32 {
    if v == Val::NONE {
        NO_REG
    } else if v.is_const() {
        (nvals + v.const_index()) as u32
    } else {
        v.0
    }
}

fn lower(f: &Func) -> Code {
    let nvals = f.vals.len();
    let mut init = vec![0u128; nvals + f.consts.len()];
    for (i, c) in f.consts.iter().enumerate() {
        init[nvals + i] = c.bits;
    }
    let r = |v: u32| reg(nvals, Val(v));
    let mut code = Code {
        name: f.name.clone(),
        ins: Vec::new(),
        init,
        moves: Vec::new(),
        regs: Vec::new(),
        cases: Vec::new(),
        st: f.blocks[0].params[0].0,
        m: f.blocks[0].params[1].0,
    };
    let layout = f.layout();
    let mut start = vec![0u32; f.blocks.len()];
    // Branch targets are block ids until every block has a position, then they are patched.
    let mut patch: Vec<usize> = Vec::new();
    for &b in &layout {
        start[b.index()] = code.ins.len() as u32;
        for i in f.insts(b) {
            if i.flags & DEAD != 0 {
                continue;
            }
            let o = i.ops;
            let d = i.result.map_or(NO_REG, |v| r(v.0));
            let ty = i.ty;
            let moves = |code: &mut Code, target: u32, args: &[u32]| {
                let start = code.moves.len() as u32;
                for (p, a) in f.blocks[target as usize].params.iter().zip(args) {
                    code.moves.push((p.0, r(*a)));
                }
                Span { start, len: args.len() as u32 }
            };
            let ins = match i.op.form() {
                Form::Un | Form::Conv => {
                    let from = f.ty(Val(o[0]));
                    Ins::Un { op: i.op, ty: from, to: i.op.result(ty), d, a: r(o[0]) }
                }
                Form::Bin | Form::Cmp | Form::Wide | Form::StrMk => {
                    let oty = i.ty;
                    if i.op == Op::Add && oty == Ty::I64 {
                        Ins::AddI64 { d, a: r(o[0]), b: r(o[1]) }
                    } else {
                        Ins::Bin { op: i.op, ty: oty, d, a: r(o[0]), b: r(o[1]) }
                    }
                }
                Form::Sel => Ins::Sel { d, c: r(o[0]), a: r(o[1]), b: r(o[2]) },
                Form::TrapBin => {
                    Ins::TrapBin { op: i.op, ty, d, a: r(o[0]), b: r(o[1]), err: o[2] }
                }
                Form::TrapUn => Ins::TrapUn { op: i.op, ty, to: ty, d, a: r(o[0]), err: o[1] },
                Form::TrapConv => {
                    Ins::TrapUn { op: i.op, ty: f.ty(Val(o[0])), to: ty, d, a: r(o[0]), err: o[1] }
                }
                Form::Scale => Ins::Scale { op: i.op, ty, d, a: r(o[0]), k: o[1], err: 0 },
                Form::TrapScale => Ins::Scale { op: i.op, ty, d, a: r(o[0]), k: o[1], err: o[2] },
                Form::EdgeBin => {
                    patch.push(code.ins.len());
                    let okreg = f.blocks[o[2] as usize].params[0].0;
                    Ins::Edge { op: i.op, ty, a: r(o[0]), b: r(o[1]), ok: o[2], okreg, ovf: o[3] }
                }
                Form::Load => {
                    Ins::Load { ty, d, base: r(o[0]), idx: r(o[1]), scale: o[2], disp: o[3] as i32 }
                }
                Form::Store => Ins::Store {
                    ty,
                    base: r(o[0]),
                    idx: r(o[1]),
                    scale: o[2],
                    disp: o[3] as i32,
                    v: r(o[4]),
                },
                Form::LoadBit => Ins::LoadBit { d, base: r(o[0]), idx: r(o[1]) },
                Form::Memcpy => Ins::Memcpy { dst: r(o[0]), src: r(o[1]), n: o[2] },
                Form::Memeq => Ins::Memeq { d, a: r(o[0]), b: r(o[1]), n: o[2] },
                Form::Prefetch => Ins::Nop,
                Form::Cas => Ins::Cas { ty, d, addr: r(o[0]), old: r(o[1]), new: r(o[2]) },
                Form::Atomic => Ins::Atomic { ty, d, addr: r(o[0]), v: r(o[1]) },
                Form::Br => {
                    patch.push(code.ins.len());
                    Ins::Jump { pc: o[0], moves: moves(&mut code, o[0], &o[1..]) }
                }
                Form::Brif => {
                    patch.push(code.ins.len());
                    let n = o[2] as usize;
                    let tm = moves(&mut code, o[1], &o[3..3 + n]);
                    let fm = moves(&mut code, o[3 + n], &o[4 + n..]);
                    Ins::Brif { c: r(o[0]), t: o[1], tm, f: o[3 + n], fm }
                }
                Form::Switch => {
                    patch.push(code.ins.len());
                    let n = o[2] as usize;
                    let dm = moves(&mut code, o[1], &o[3..3 + n]);
                    let cstart = code.cases.len() as u32;
                    for pair in o[3 + n..].as_chunks::<2>().0 {
                        code.cases.push((u128::from(pair[0]), pair[1]));
                    }
                    let cases = Span { start: cstart, len: code.cases.len() as u32 - cstart };
                    Ins::Switch { x: r(o[0]), default: o[1], dm, cases }
                }
                Form::Ret => Ins::Ret { a: r(o[0]) },
                Form::Trap => Ins::Trap { err: o[0] },
                Form::Rtcall => {
                    let start = code.regs.len() as u32;
                    code.regs.extend(o[1..].iter().map(|w| r(*w)));
                    Ins::Rtcall { proxy: o[0], d, args: Span { start, len: o.len() as u32 - 1 } }
                }
                Form::Vcall => {
                    let start = code.regs.len() as u32;
                    code.regs.extend(o[2..].iter().map(|w| r(*w)));
                    Ins::Vcall {
                        kernel: o[0],
                        n: r(o[1]),
                        bufs: Span { start, len: o.len() as u32 - 2 },
                    }
                }
                Form::Guard => Ins::Guard { c: r(o[0]), g: o[1] },
                Form::Poll => Ins::Poll { n: o[0].max(1) },
                Form::CtrAdd => Ins::CtrAdd { k: o[0], v: r(o[1]) },
            };
            code.ins.push(ins);
        }
    }
    let pc = |b: u32| start[b as usize];
    for at in patch {
        code.ins[at] = match code.ins[at] {
            Ins::Jump { pc: t, moves } => Ins::Jump { pc: pc(t), moves },
            Ins::Brif { c, t, tm, f, fm } => Ins::Brif { c, t: pc(t), tm, f: pc(f), fm },
            Ins::Switch { x, default, dm, cases } => {
                for k in cases.start..cases.start + cases.len {
                    let (key, b) = code.cases[k as usize];
                    code.cases[k as usize] = (key, pc(b));
                }
                Ins::Switch { x, default: pc(default), dm, cases }
            }
            Ins::Edge { op, ty, a, b, ok, okreg, ovf } => {
                Ins::Edge { op, ty, a, b, ok: pc(ok), okreg, ovf: pc(ovf) }
            }
            other => other,
        };
    }
    code
}

fn addr(r: &[u128], base: u32, idx: u32, scale: u32, disp: i32) -> usize {
    let mut a = r[base as usize] as usize;
    if idx != NO_REG {
        a = a.wrapping_add((r[idx as usize] as u64 as usize).wrapping_mul(scale as usize));
    }
    a.wrapping_add(disp as isize as usize)
}

/// Reads `ty` at `a`.
#[allow(unsafe_code)]
fn load(ty: Ty, a: usize) -> u128 {
    let p = std::ptr::with_exposed_provenance::<u8>(a);
    // SAFETY: the generator only emits loads from state, morsel and runtime memory that is
    // live and at least as large as the access for the duration of the call, and the verifier's
    // V6 bounds the state accesses. Unaligned reads are allowed by `read_unaligned`.
    unsafe {
        match ty {
            Ty::I1 => u128::from(p.read() & 1),
            Ty::I8 => u128::from(p.read()),
            Ty::I16 => u128::from(p.cast::<u16>().read_unaligned()),
            Ty::I32 | Ty::F32 => u128::from(p.cast::<u32>().read_unaligned()),
            Ty::I64 | Ty::F64 | Ty::Ptr => u128::from(p.cast::<u64>().read_unaligned()),
            Ty::I128 | Ty::Str16 => p.cast::<u128>().read_unaligned(),
            Ty::Void => 0,
        }
    }
}

/// Writes `v` as `ty` at `a`.
#[allow(unsafe_code)]
fn store(ty: Ty, a: usize, v: u128) {
    let p = std::ptr::with_exposed_provenance_mut::<u8>(a);
    // SAFETY: as for `load`, the generator only stores into live state and runtime memory it
    // owns for the duration of the call.
    unsafe {
        match ty {
            Ty::I1 => p.write((v & 1) as u8),
            Ty::I8 => p.write(v as u8),
            Ty::I16 => p.cast::<u16>().write_unaligned(v as u16),
            Ty::I32 | Ty::F32 => p.cast::<u32>().write_unaligned(v as u32),
            Ty::I64 | Ty::F64 | Ty::Ptr => p.cast::<u64>().write_unaligned(v as u64),
            Ty::I128 | Ty::Str16 => p.cast::<u128>().write_unaligned(v),
            Ty::Void => {}
        }
    }
}

#[allow(unsafe_code)]
fn bytes<'a>(a: usize, n: u32) -> &'a [u8] {
    // SAFETY: `memcpy` and `memeq` operands are `n` live bytes by the generator's construction,
    // and V8 bounds `n` by 64.
    unsafe { std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(a), n as usize) }
}

#[allow(unsafe_code)]
fn copy(dst: usize, src: usize, n: u32) {
    // SAFETY: as for `bytes`; `copy` allows the ranges to overlap.
    unsafe {
        std::ptr::copy(
            std::ptr::with_exposed_provenance::<u8>(src),
            std::ptr::with_exposed_provenance_mut::<u8>(dst),
            n as usize,
        );
    }
}

fn apply(code: &Code, r: &mut [u128], m: Span, scratch: &mut Vec<u128>) {
    let moves = &code.moves[m.start as usize..(m.start + m.len) as usize];
    match moves {
        [] => {}
        [(d, s)] => r[*d as usize] = r[*s as usize],
        _ => {
            // Block arguments move in parallel: read every source before writing any target.
            scratch.clear();
            scratch.extend(moves.iter().map(|(_, s)| r[*s as usize]));
            for ((d, _), v) in moves.iter().zip(scratch.iter()) {
                r[*d as usize] = *v;
            }
        }
    }
}

fn run(code: &Code, r: &mut [u128], rt: &mut dyn Runtime) -> u64 {
    let mut pc = 0usize;
    let mut scratch = Vec::new();
    let mut polls = 0u32;
    loop {
        let ins = code.ins[pc];
        pc += 1;
        match ins {
            Ins::AddI64 { d, a, b } => {
                r[d as usize] =
                    u128::from((r[a as usize] as u64).wrapping_add(r[b as usize] as u64))
            }
            Ins::Un { op, ty, to, d, a } => {
                r[d as usize] = eval::unary(op, ty, to, r[a as usize])
                    .expect("the verifier checked the opcode");
            }
            Ins::Bin { op, ty, d, a, b } => {
                r[d as usize] = eval::binary(op, ty, r[a as usize], r[b as usize])
                    .expect("the verifier checked the opcode");
            }
            Ins::Sel { d, c, a, b } => {
                r[d as usize] = if r[c as usize] & 1 != 0 { r[a as usize] } else { r[b as usize] }
            }
            Ins::TrapBin { op, ty, d, a, b, err } => {
                match eval::binary(op, ty, r[a as usize], r[b as usize]) {
                    Ok(v) => r[d as usize] = v,
                    Err(_) => return status::make(status::ERROR, u64::from(err)),
                }
            }
            Ins::TrapUn { op, ty, to, d, a, err } => match eval::unary(op, ty, to, r[a as usize]) {
                Ok(v) => r[d as usize] = v,
                Err(_) => return status::make(status::ERROR, u64::from(err)),
            },
            Ins::Scale { op, ty, d, a, k, err } => match eval::scale(op, ty, r[a as usize], k) {
                Ok(v) => r[d as usize] = v,
                Err(Fault::Trap | Fault::NotPure) => {
                    return status::make(status::ERROR, u64::from(err));
                }
            },
            Ins::Edge { op, ty, a, b, ok, okreg, ovf } => {
                match eval::binary(op, ty, r[a as usize], r[b as usize]) {
                    Ok(v) => {
                        r[okreg as usize] = v;
                        pc = ok as usize;
                    }
                    Err(_) => pc = ovf as usize,
                }
            }
            Ins::Load { ty, d, base, idx, scale, disp } => {
                r[d as usize] = load(ty, addr(r, base, idx, scale, disp))
            }
            Ins::Store { ty, base, idx, scale, disp, v } => {
                store(ty, addr(r, base, idx, scale, disp), r[v as usize])
            }
            Ins::LoadBit { d, base, idx } => {
                let i = r[idx as usize] as u64 as usize;
                let byte = load(Ty::I8, (r[base as usize] as usize).wrapping_add(i / 8));
                r[d as usize] = (byte >> (i % 8)) & 1;
            }
            Ins::Memcpy { dst, src, n } => {
                copy(r[dst as usize] as usize, r[src as usize] as usize, n)
            }
            Ins::Memeq { d, a, b, n } => {
                r[d as usize] = u128::from(
                    bytes(r[a as usize] as usize, n) == bytes(r[b as usize] as usize, n),
                );
            }
            Ins::Cas { ty, d, addr: at, old, new } => {
                // One thread per state: the interpreter runs a worker's calls in order, and shared
                // structures reached by `cas` are only raced by other workers, which the runtime
                // gives their own interpreter. A plain compare and write is the single thread case.
                let a = r[at as usize] as usize;
                let cur = load(ty, a);
                let hit = cur == r[old as usize] & ty.mask();
                if hit {
                    store(ty, a, r[new as usize]);
                }
                r[d as usize] = u128::from(hit);
            }
            Ins::Atomic { ty, d, addr: at, v } => {
                let a = r[at as usize] as usize;
                let cur = load(ty, a);
                store(ty, a, cur.wrapping_add(r[v as usize]));
                r[d as usize] = cur;
            }
            Ins::Jump { pc: t, moves } => {
                apply(code, r, moves, &mut scratch);
                pc = t as usize;
            }
            Ins::Brif { c, t, tm, f, fm } => {
                if r[c as usize] & 1 != 0 {
                    apply(code, r, tm, &mut scratch);
                    pc = t as usize;
                } else {
                    apply(code, r, fm, &mut scratch);
                    pc = f as usize;
                }
            }
            Ins::Switch { x, default, dm, cases } => {
                let key = r[x as usize];
                let hit = code.cases[cases.start as usize..(cases.start + cases.len) as usize]
                    .iter()
                    .find(|(k, _)| *k == key);
                match hit {
                    Some((_, t)) => pc = *t as usize,
                    None => {
                        apply(code, r, dm, &mut scratch);
                        pc = default as usize;
                    }
                }
            }
            Ins::Ret { a } => return r[a as usize] as u64,
            Ins::Trap { err } => return status::make(status::ERROR, u64::from(err)),
            Ins::Rtcall { proxy, d, args } => {
                scratch.clear();
                scratch.extend(
                    code.regs[args.start as usize..(args.start + args.len) as usize]
                        .iter()
                        .map(|x| r[*x as usize]),
                );
                match rt.rtcall(proxy, &scratch) {
                    Ok(v) => {
                        if d != NO_REG {
                            r[d as usize] = v;
                        }
                    }
                    Err(s) => return s,
                }
            }
            Ins::Vcall { kernel, n, bufs } => {
                scratch.clear();
                scratch.extend(
                    code.regs[bufs.start as usize..(bufs.start + bufs.len) as usize]
                        .iter()
                        .map(|x| r[*x as usize]),
                );
                if let Err(s) = rt.vcall(kernel, r[n as usize] as u64, &scratch) {
                    return s;
                }
            }
            Ins::Guard { c, g } => {
                if r[c as usize] & 1 == 0 {
                    return status::make(status::DEOPT, u64::from(g));
                }
            }
            Ins::Poll { n } => {
                polls += 1;
                if polls >= n {
                    polls = 0;
                    if rt.cancelled() {
                        return status::make(status::CANCELLED, 0);
                    }
                }
            }
            Ins::CtrAdd { k, v } => rt.count(k, r[v as usize] as u64),
            Ins::Nop => {}
        }
    }
}

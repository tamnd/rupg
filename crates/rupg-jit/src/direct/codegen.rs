// Lifted from rudb crates/rudb-qc-direct/src/codegen.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the tier are under `crate::direct` and QIR is `crate::ir`. The cases of a switch are read with `as_chunks`.

//! The code generation pass of section 8.5: instruction selection, register allocation and
//! encoding in one walk over the blocks in layout order.
//!
//! Every value has a home decided before the walk. `%st` and `%m` live in `r15` and `r14`, a
//! value the analysis hinted lives in `rbx`, `r12` or `r13` for its whole life, and every other
//! value gets a stack slot, shared with values whose intervals do not meet. The seven caller
//! saved registers left, `rax` to `r9`, are a cache over the slots. A value that dies in the
//! block that defines it stays in the cache without being stored until the cache needs the
//! register, and any other value is stored when it is defined and kept in the cache clean. The
//! cache is empty at the start of every block and after every call, so what a block expects of
//! the registers never depends on where it was entered from. `r10` and `r11` are scratch inside
//! one instruction.
//!
//! The frame, from `rbp` down: the five callee saved registers, the `poll` counter, two 16 byte
//! scratch words for wide constants, the slots, and at `rsp` the call area, which holds a
//! runtime call's result word and then its arguments, the operands of an `eval` helper, or the
//! values a branch copies before it writes the block parameters they overlap.
//!
//! What the code computes is what `crate::ir::eval` computes. An operation on a type the
//! patterns below do not cover goes to the `eval` helper, which is the interpreter's own code.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use crate::ir::entry::{CTX_OFFSET, Entry};
use crate::ir::eval::sext as sext_bits;
use crate::ir::{Block, Form, Func, Inst, Op, Ty, Val, status};

use crate::direct::analysis::{self, Analysis, PINS};
use crate::direct::asm::{Alu, Asm, Bits, Cc, Fop, Label, Mem, Prec, Reg, Shift, Size, Unary, Xmm};
use crate::direct::{Backend, Error, Function, Reloc, fail};

/// The cache: the caller saved registers the calling convention leaves.
const POOL: [Reg; 7] = [Reg::RAX, Reg::RCX, Reg::RDX, Reg::RSI, Reg::RDI, Reg::R8, Reg::R9];
/// The registers the analysis hands out, by hint number less one.
const PINNED: [Reg; PINS] = [Reg::RBX, Reg::R12, Reg::R13];
/// Scratch inside one instruction.
const T0: Reg = Reg::R10;
const T1: Reg = Reg::R11;
/// Where `%st` and `%m` live.
const ST: Reg = Reg::R15;
const MORSEL: Reg = Reg::R14;

/// The `poll` counter, a dword.
const POLLS: i32 = -48;
/// The two scratch words a wide constant is written to when it has to be in memory.
const KONST: [i32; 2] = [-64, -80];
/// The bytes from `rbp` down to the first slot.
const FIXED: i32 = 80;
/// The callee saved registers pushed after `rbp`.
const SAVED: i32 = 40;
/// The largest frame, which no real function comes near.
const MAX_FRAME: i32 = 1 << 20;
/// The call area: the result word at `rsp`, then 16 byte argument words.
const ARGS: i32 = 16;

const X0: Xmm = Xmm(0);
const X1: Xmm = Xmm(1);
const X15: Xmm = Xmm(15);

/// No value, in the cache tables.
const EMPTY: u32 = u32::MAX;
/// Not in a register.
const NOREG: u8 = u8::MAX;

/// Where a value lives between instructions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Home {
    /// Never defined in a reachable block.
    None,
    /// A callee saved register, for the value's whole life.
    Reg(Reg),
    /// A stack slot at this offset from `rbp`.
    Slot(i32),
}

/// Where a block argument is read from.
#[derive(Clone, Copy, Debug)]
enum Src {
    Reg(Reg),
    Mem(Mem),
    Imm(u128),
}

/// A `poll` whose slow path is emitted after the blocks.
struct Poll {
    stub: Label,
    back: Label,
    /// The cache registers holding a value at the `poll`, which the slow path saves.
    live: u16,
}

fn wide(t: Ty) -> bool {
    t.bits() > 64
}

fn narrow(t: Ty) -> bool {
    (1..=64).contains(&t.bits())
}

fn size(t: Ty) -> Size {
    match t.bits() {
        0..=8 => Size::B,
        16 => Size::W,
        32 => Size::D,
        _ => Size::Q,
    }
}

fn prec(t: Ty) -> Prec {
    if t == Ty::F32 { Prec::S } else { Prec::D }
}

fn pool(r: Reg) -> bool {
    matches!(r.0, 0..=2 | 6..=9)
}

fn bit(r: Reg) -> u16 {
    1 << r.0
}

fn hi(m: Mem) -> Mem {
    Mem { disp: m.disp + 8, ..m }
}

fn slot(disp: i32) -> Mem {
    Mem::at(Reg::RBP, disp)
}

fn arg(k: usize) -> Mem {
    Mem::at(Reg::RSP, ARGS + 16 * k as i32)
}

/// Compiles one function.
pub(crate) fn compile(be: &Backend, f: &Func) -> Result<Function, Error> {
    let an = Analysis::new(f)
        .map_err(|e| fail(&f.name, format!("irreducible control flow into block {}", e.block.0)))?;
    match f.blocks.first() {
        Some(b) if b.params.len() == 2 => {}
        _ => return Err(fail(&f.name, "an entry block without (%st, %m)")),
    }
    let mut g = Gen::new(be, f, an);
    let slots = g.homes();
    g.emit(slots)?;
    let Gen { asm, relocs, .. } = g;
    Ok(Function { name: f.name.clone(), bytes: asm.code, relocs })
}

struct Gen<'a> {
    be: &'a Backend,
    f: &'a Func,
    an: Analysis,
    asm: Asm,
    home: Vec<Home>,
    labels: Vec<Label>,
    /// The block after the one being emitted, which a branch can fall into.
    next: Option<Block>,
    /// The position in the analysis order of the block being emitted.
    pos: u32,
    /// The value each register holds, or `EMPTY`.
    holder: [u32; 16],
    /// The register each value is in, or `NOREG`.
    inreg: Vec<u8>,
    /// Registers holding a value their slot does not have yet.
    dirty: u16,
    /// Registers the current instruction is using.
    locked: u16,
    stamp: [u32; 16],
    clock: u32,
    /// For a value that dies in the block that defines it, the reads it has left.
    rem: Vec<u32>,
    local: Vec<bool>,
    /// The call area's argument words the function needs.
    words: usize,
    exits: Vec<(u64, Label)>,
    polls: Vec<Poll>,
    lits: [Option<Label>; Entry::ALL.len()],
    relocs: Vec<Reloc>,
    epilogue: Label,
    seen: Vec<u32>,
    generation: u32,
    buf: Vec<Val>,
}

impl<'a> Gen<'a> {
    fn new(be: &'a Backend, f: &'a Func, an: Analysis) -> Gen<'a> {
        let n = f.vals.len();
        let mut asm = Asm::new();
        let labels = (0..f.blocks.len()).map(|_| asm.label()).collect();
        let epilogue = asm.label();
        Gen {
            be,
            f,
            an,
            asm,
            home: vec![Home::None; n],
            labels,
            next: None,
            pos: 0,
            holder: [EMPTY; 16],
            inreg: vec![NOREG; n],
            dirty: 0,
            locked: 0,
            stamp: [0; 16],
            clock: 0,
            rem: vec![0; n],
            local: vec![false; n],
            words: 2,
            exits: Vec::new(),
            polls: Vec::new(),
            lits: [None; Entry::ALL.len()],
            relocs: Vec::new(),
            epilogue,
            seen: vec![0; n],
            generation: 0,
            buf: Vec::new(),
        }
    }

    fn fail(&self, reason: impl Into<String>) -> Error {
        fail(&self.f.name, reason)
    }

    /// Gives every value its home, and returns the bytes of slots. The slots are handed out by a
    /// linear scan over the intervals in order of their start, so a slot is reused once every
    /// value in it is dead.
    fn homes(&mut self) -> i32 {
        let (f, an) = (self.f, &self.an);
        let entry = &f.blocks[0].params;
        self.home[entry[0].index()] = Home::Reg(ST);
        self.home[entry[1].index()] = Home::Reg(MORSEL);
        let npos = an.order.len();
        let mut count = vec![0u32; npos + 1];
        for v in 0..f.vals.len() {
            let s = an.start[v];
            if s == analysis::NONE || self.home[v] != Home::None {
                continue;
            }
            if an.pin[v] > 0 {
                self.home[v] = Home::Reg(PINNED[an.pin[v] as usize - 1]);
                continue;
            }
            count[s as usize + 1] += 1;
        }
        for p in 0..npos {
            count[p + 1] += count[p];
        }
        let mut sorted = vec![0u32; count[npos] as usize];
        let mut fill = count.clone();
        for v in 0..f.vals.len() {
            let s = an.start[v];
            if s == analysis::NONE || self.home[v] != Home::None {
                continue;
            }
            sorted[fill[s as usize] as usize] = v as u32;
            fill[s as usize] += 1;
        }
        let mut active: BinaryHeap<Reverse<(u32, i32, bool)>> = BinaryHeap::new();
        let (mut free8, mut free16) = (Vec::new(), Vec::new());
        let mut top = 0i32;
        for v in sorted {
            let v = v as usize;
            let s = an.start[v];
            while let Some(&Reverse((e, disp, w))) = active.peek() {
                if e >= s {
                    break;
                }
                active.pop();
                if w { free16.push(disp) } else { free8.push(disp) }
            }
            let w = wide(f.vals[v].ty);
            let free = if w { &mut free16 } else { &mut free8 };
            let disp = free.pop().unwrap_or_else(|| {
                top += if w { 16 } else { 8 };
                -(FIXED + top)
            });
            self.home[v] = Home::Slot(disp);
            active.push(Reverse((an.end[v], disp, w)));
        }
        top
    }

    fn emit(&mut self, slots: i32) -> Result<(), Error> {
        let a = &mut self.asm;
        a.push(Reg::RBP);
        a.mov(Size::Q, Reg::RBP, Reg::RSP);
        for r in [Reg::RBX, Reg::R12, Reg::R13, Reg::R14, Reg::R15] {
            a.push(r);
        }
        // The frame size is known at the end, so this takes the four byte immediate for now.
        a.alu_imm(Alu::Sub, Size::Q, Reg::RSP, 0x1000_0000);
        let frame_at = a.len() - 4;
        a.mov(Size::Q, ST, Reg::RDI);
        a.mov(Size::Q, MORSEL, Reg::RSI);
        a.store_imm(Size::D, slot(POLLS), 0);
        let layout = std::mem::take(&mut self.an.layout);
        if layout.first() != Some(&Block(0)) {
            self.asm.jmp(self.labels[0]);
        }
        let f = self.f;
        for (at, &b) in layout.iter().enumerate() {
            self.next = layout.get(at + 1).copied();
            self.pos = self.an.pos[b.index()];
            self.asm.bind(self.labels[b.index()]);
            self.forget();
            match f.terminator(b) {
                Some(t) if t.op.is_terminator() => {}
                _ => return Err(self.fail(format!("block {} has no terminator", b.0))),
            }
            for i in f.insts(b) {
                if i.dead() {
                    continue;
                }
                self.inst(&i)?;
                self.release(&i);
            }
        }
        self.an.layout = layout;
        self.tail();
        let frame = FIXED - SAVED + slots + ARGS + 16 * self.words as i32;
        let frame = frame + (24 - frame % 16) % 16;
        if frame > MAX_FRAME {
            return Err(self.fail(format!("a frame of {frame} bytes")));
        }
        self.asm.patch32(frame_at, frame as u32);
        self.asm.finish().map_err(|l| self.fail(format!("label {} is never placed", l.0)))
    }

    /// The `poll` slow paths, the exits, the epilogue and the literal table.
    fn tail(&mut self) {
        for p in std::mem::take(&mut self.polls) {
            let a = &mut self.asm;
            a.bind(p.stub);
            a.store_imm(Size::D, slot(POLLS), 0);
            let saved: Vec<Reg> = POOL.iter().copied().filter(|r| p.live & bit(*r) != 0).collect();
            for &r in &saved {
                a.push(r);
            }
            let odd = saved.len() % 2 == 1;
            if odd {
                a.alu_imm(Alu::Sub, Size::Q, Reg::RSP, 8);
            }
            a.load(Size::Q, Reg::RDI, Mem::at(ST, CTX_OFFSET));
            self.call(Entry::Cancelled);
            let a = &mut self.asm;
            a.mov(Size::D, T0, Reg::RAX);
            if odd {
                a.alu_imm(Alu::Add, Size::Q, Reg::RSP, 8);
            }
            for &r in saved.iter().rev() {
                a.pop(r);
            }
            a.test(Size::D, T0, T0);
            let out = self.exit(status::make(status::CANCELLED, 0));
            self.asm.jcc(Cc::Ne, out);
            self.asm.jmp(p.back);
        }
        let mut exits = std::mem::take(&mut self.exits);
        exits.sort_by_key(|e| e.0);
        let a = &mut self.asm;
        for (s, l) in exits {
            a.bind(l);
            a.mov_imm(Reg::RAX, s);
            a.jmp(self.epilogue);
        }
        a.bind(self.epilogue);
        a.lea(Reg::RSP, Mem::at(Reg::RBP, -SAVED));
        for r in [Reg::R15, Reg::R14, Reg::R13, Reg::R12, Reg::RBX, Reg::RBP] {
            a.pop(r);
        }
        a.ret();
        a.align(8);
        for e in Entry::ALL {
            if let Some(l) = self.lits[e.index() as usize] {
                self.asm.bind(l);
                self.relocs.push(Reloc { offset: self.asm.len() as u32, entry: e, addend: 0 });
                for _ in 0..8 {
                    self.asm.byte(0);
                }
            }
        }
    }

    // Values.

    fn ty(&self, w: u32) -> Ty {
        self.f.ty(Val(w))
    }

    fn konst(&self, w: u32) -> Option<u128> {
        let v = Val(w);
        v.is_const().then(|| self.f.consts[v.const_index()].bits)
    }

    /// A constant as the immediate of a 64 bit instruction, when it is one.
    fn imm64(&self, w: u32) -> Option<i32> {
        self.konst(w).and_then(|c| i32::try_from(c as u64 as i64).ok())
    }

    /// A constant of type `t` sign extended from its width, as an immediate, when it fits. Right
    /// for an instruction whose result is masked to the width after, or that runs at the width.
    fn imm(&self, w: u32, t: Ty) -> Option<i32> {
        self.konst(w).and_then(|c| i32::try_from(sext_bits(t, c)).ok())
    }

    fn result(&self, i: &Inst<'_>) -> Result<Val, Error> {
        i.result.ok_or_else(|| self.fail(format!("{} with no result", i.op.name())))
    }

    fn slot_of(&self, v: Val) -> Result<Mem, Error> {
        match self.home[v.index()] {
            Home::Slot(d) => Ok(slot(d)),
            _ => Err(self.fail(format!("v{} has no slot", v.0))),
        }
    }

    /// Where the sixteen bytes of a wide value are. A constant is written to scratch word `k`.
    fn mem(&mut self, w: u32, k: usize) -> Result<Mem, Error> {
        if let Some(c) = self.konst(w) {
            let m = slot(KONST[k]);
            self.store_const(Size::Q, m, c as u64);
            self.store_const(Size::Q, hi(m), (c >> 64) as u64);
            return Ok(m);
        }
        self.slot_of(Val(w))
    }

    fn store_const(&mut self, s: Size, m: Mem, c: u64) {
        if s != Size::Q {
            self.asm.store_imm(s, m, c as u32 as i32);
        } else if let Ok(x) = i32::try_from(c as i64) {
            self.asm.store_imm(Size::Q, m, x);
        } else {
            self.asm.mov_imm(T0, c);
            self.asm.store(Size::Q, m, T0);
        }
    }

    // The register cache.

    fn touch(&mut self, r: Reg) {
        self.clock += 1;
        self.stamp[r.0 as usize] = self.clock;
    }

    /// Empties `r`, storing what it holds if that is still wanted and not in its slot.
    fn evict(&mut self, r: Reg) {
        let x = self.holder[r.0 as usize];
        if x == EMPTY {
            return;
        }
        if self.dirty & bit(r) != 0
            && self.rem[x as usize] > 0
            && let Home::Slot(d) = self.home[x as usize]
        {
            self.asm.store(Size::Q, slot(d), r);
        }
        self.inreg[x as usize] = NOREG;
        self.holder[r.0 as usize] = EMPTY;
        self.dirty &= !bit(r);
    }

    /// A cache register for the current instruction to write: an empty one if there is one, else
    /// the least recently used clean one, else the least recently used one.
    fn fresh(&mut self) -> Result<Reg, Error> {
        let mut best = None;
        let mut score = u64::MAX;
        for r in POOL {
            if self.locked & bit(r) != 0 {
                continue;
            }
            let s = if self.holder[r.0 as usize] == EMPTY {
                0
            } else if self.dirty & bit(r) == 0 {
                1 + u64::from(self.stamp[r.0 as usize])
            } else {
                (1 << 40) + u64::from(self.stamp[r.0 as usize])
            };
            if s < score {
                score = s;
                best = Some(r);
                if s == 0 {
                    break;
                }
            }
        }
        let r =
            best.ok_or_else(|| self.fail("an instruction that needs more than seven registers"))?;
        self.evict(r);
        self.locked |= bit(r);
        self.touch(r);
        Ok(r)
    }

    /// Takes `r` for the current instruction, whatever it holds. The caller has copied out any
    /// operand it had in `r`.
    fn claim(&mut self, r: Reg) {
        self.evict(r);
        self.locked |= bit(r);
    }

    /// A register holding the low 64 bits of `w`, not to be written.
    fn reg(&mut self, w: u32) -> Result<Reg, Error> {
        if let Some(c) = self.konst(w) {
            let r = self.fresh()?;
            self.asm.mov_imm(r, c as u64);
            return Ok(r);
        }
        let x = w as usize;
        match self.home[x] {
            Home::Reg(r) => Ok(r),
            Home::Slot(d) if wide(self.f.vals[x].ty) => {
                let r = self.fresh()?;
                self.asm.load(Size::Q, r, slot(d));
                Ok(r)
            }
            Home::Slot(d) => {
                if self.inreg[x] != NOREG {
                    let r = Reg(self.inreg[x]);
                    self.locked |= bit(r);
                    self.touch(r);
                    return Ok(r);
                }
                let r = self.fresh()?;
                self.asm.load(Size::Q, r, slot(d));
                self.holder[r.0 as usize] = w;
                self.inreg[x] = r.0;
                Ok(r)
            }
            Home::None => Err(self.fail(format!("v{w} is used but never defined"))),
        }
    }

    /// A cache register holding the low 64 bits of `w` that the current instruction may write.
    fn take(&mut self, w: u32) -> Result<Reg, Error> {
        if !Val(w).is_const() && matches!(self.home[w as usize], Home::Slot(_)) {
            let x = w as usize;
            let r = self.inreg[x];
            if r != NOREG {
                let r = Reg(r);
                let free = self.locked & bit(r) == 0
                    && (self.dirty & bit(r) == 0 || (self.local[x] && self.rem[x] == 1));
                if free {
                    self.inreg[x] = NOREG;
                    self.holder[r.0 as usize] = EMPTY;
                    self.dirty &= !bit(r);
                    self.locked |= bit(r);
                    self.touch(r);
                    return Ok(r);
                }
                let d = self.fresh()?;
                self.asm.mov(Size::Q, d, r);
                return Ok(d);
            }
        }
        match self.home.get(w as usize).copied() {
            Some(Home::Reg(p)) if !Val(w).is_const() => {
                let d = self.fresh()?;
                self.asm.mov(Size::Q, d, p);
                Ok(d)
            }
            _ if Val(w).is_const() => self.reg(w),
            Some(Home::Slot(d)) => {
                let r = self.fresh()?;
                self.asm.load(Size::Q, r, slot(d));
                Ok(r)
            }
            _ => Err(self.fail(format!("v{w} is used but never defined"))),
        }
    }

    /// Where `w` can be read from without changing the cache, for a branch's moves.
    fn peek(&self, w: u32) -> Result<Src, Error> {
        if let Some(c) = self.konst(w) {
            return Ok(Src::Imm(c));
        }
        let x = w as usize;
        match self.home[x] {
            Home::Reg(r) => Ok(Src::Reg(r)),
            Home::Slot(_) if self.inreg[x] != NOREG => Ok(Src::Reg(Reg(self.inreg[x]))),
            Home::Slot(d) => Ok(Src::Mem(slot(d))),
            Home::None => Err(self.fail(format!("v{w} is used but never defined"))),
        }
    }

    /// `v` is the value in `r`.
    fn define(&mut self, v: Val, r: Reg) {
        let x = v.index();
        match self.home[x] {
            Home::Reg(p) => {
                if p != r {
                    self.asm.mov(Size::Q, p, r);
                }
            }
            Home::Slot(d) => {
                let local = !self.an.live_out(v, self.pos);
                self.local[x] = local;
                self.rem[x] = self.an.last[x];
                if !pool(r) {
                    self.asm.store(Size::Q, slot(d), r);
                    return;
                }
                if self.holder[r.0 as usize] != EMPTY {
                    self.evict(r);
                }
                if local {
                    if self.rem[x] == 0 {
                        return;
                    }
                    self.dirty |= bit(r);
                } else {
                    self.asm.store(Size::Q, slot(d), r);
                }
                self.holder[r.0 as usize] = v.0;
                self.inreg[x] = r.0;
                self.touch(r);
            }
            Home::None => {}
        }
    }

    /// `v` is the sixteen bytes at `m`.
    fn define_wide(&mut self, v: Val, m: Mem) -> Result<(), Error> {
        let d = self.slot_of(v)?;
        self.asm.vload(X15, m);
        self.asm.vstore(d, X15);
        Ok(())
    }

    /// `v` is the value of type `t` at `m`, whatever its width.
    fn define_from(&mut self, v: Val, m: Mem, t: Ty) -> Result<(), Error> {
        if wide(t) {
            return self.define_wide(v, m);
        }
        let d = self.fresh()?;
        self.asm.load(size(t), d, m);
        if t == Ty::I1 {
            self.asm.alu_imm(Alu::And, Size::D, d, 1);
        }
        self.define(v, d);
        Ok(())
    }

    /// After an instruction: its operands are read once more, the ones that are dead leave the
    /// cache, and the registers it used are free.
    fn release(&mut self, i: &Inst<'_>) {
        let mut buf = std::mem::take(&mut self.buf);
        buf.clear();
        i.uses(|v| buf.push(v));
        for &v in &buf {
            if v == Val::NONE || v.is_const() {
                continue;
            }
            let x = v.index();
            if self.local[x] && self.rem[x] > 0 {
                self.rem[x] -= 1;
                if self.rem[x] == 0 && self.inreg[x] != NOREG {
                    let r = self.inreg[x];
                    self.holder[r as usize] = EMPTY;
                    self.dirty &= !(1 << r);
                    self.inreg[x] = NOREG;
                }
            }
        }
        self.buf = buf;
        self.locked = 0;
    }

    /// Stores every value that is only in a register, before a call.
    fn flush(&mut self) {
        for r in POOL {
            let x = self.holder[r.0 as usize];
            if x != EMPTY
                && self.dirty & bit(r) != 0
                && self.rem[x as usize] > 0
                && let Home::Slot(d) = self.home[x as usize]
            {
                self.asm.store(Size::Q, slot(d), r);
            }
        }
        self.dirty = 0;
    }

    /// Empties the cache.
    fn forget(&mut self) {
        for r in POOL {
            let x = self.holder[r.0 as usize];
            if x != EMPTY {
                self.inreg[x as usize] = NOREG;
                self.holder[r.0 as usize] = EMPTY;
            }
        }
        self.dirty = 0;
        self.locked = 0;
    }

    // Small pieces.

    /// Clears the bits of `r` above the width of `t`.
    fn norm(&mut self, t: Ty, r: Reg) {
        match t.bits() {
            1 => self.asm.alu_imm(Alu::And, Size::D, r, 1),
            8 => self.asm.movzx(Size::B, r, r),
            16 => self.asm.movzx(Size::W, r, r),
            32 => self.asm.mov(Size::D, r, r),
            _ => {}
        }
    }

    /// Sign extends `r` from the width of `t` to 64 bits.
    fn sext(&mut self, t: Ty, r: Reg) {
        match t.bits() {
            1 => self.asm.unary(Unary::Neg, Size::Q, r),
            8 => self.asm.movsx(Size::B, Size::Q, r, r),
            16 => self.asm.movsx(Size::W, Size::Q, r, r),
            32 => self.asm.movsx(Size::D, Size::Q, r, r),
            _ => {}
        }
    }

    /// Copies the low 64 bits of `w` to `d`, which is not a cache register.
    fn copy_to(&mut self, d: Reg, w: u32) -> Result<(), Error> {
        if let Some(c) = self.konst(w) {
            self.asm.mov_imm(d, c as u64);
        } else {
            let r = self.reg(w)?;
            self.asm.mov(Size::Q, d, r);
        }
        Ok(())
    }

    /// Stores the low 64 bits of `w` at `m`.
    fn store_lo(&mut self, m: Mem, w: u32) -> Result<(), Error> {
        if let Some(c) = self.konst(w) {
            self.store_const(Size::Q, m, c as u64);
        } else {
            let r = self.reg(w)?;
            self.asm.store(Size::Q, m, r);
            if self.locked & bit(r) != 0 && pool(r) {
                self.locked &= !bit(r);
            }
        }
        Ok(())
    }

    /// Writes a 16 byte call area word from `w`.
    fn spill(&mut self, w: u32, m: Mem) -> Result<(), Error> {
        if wide(self.ty(w)) {
            let s = self.mem(w, 0)?;
            self.asm.vload(X15, s);
            self.asm.vstore(m, X15);
        } else {
            self.store_lo(m, w)?;
            self.asm.store_imm(Size::Q, hi(m), 0);
        }
        Ok(())
    }

    fn exit(&mut self, s: u64) -> Label {
        if let Some(&(_, l)) = self.exits.iter().find(|e| e.0 == s) {
            return l;
        }
        let l = self.asm.label();
        self.exits.push((s, l));
        l
    }

    fn error(&mut self, site: u32) -> Label {
        self.exit(status::make(status::ERROR, u64::from(site)))
    }

    fn call(&mut self, e: Entry) {
        let at = e.index() as usize;
        let l = match self.lits[at] {
            Some(l) => l,
            None => {
                let l = self.asm.label();
                self.lits[at] = Some(l);
                l
            }
        };
        self.asm.call_rip(l);
    }

    fn jump(&mut self, b: Block) {
        if self.next != Some(b) {
            self.asm.jmp(self.labels[b.index()]);
        }
    }

    /// Calls an `eval` helper on `args`, leaving the result at `rsp + ARGS`. With a site, a trap
    /// returns the error.
    fn eval(
        &mut self,
        e: Entry,
        op: Op,
        ty: Ty,
        extra: Option<u32>,
        args: &[u32],
        site: Option<u32>,
    ) -> Result<Mem, Error> {
        for (k, &w) in args.iter().enumerate() {
            self.spill(w, arg(k))?;
        }
        self.flush();
        self.forget();
        let a = &mut self.asm;
        a.mov_imm(Reg::RDI, op as u64);
        a.mov_imm(Reg::RSI, ty as u64);
        match extra {
            Some(x) => {
                a.mov_imm(Reg::RDX, u64::from(x));
                a.lea(Reg::RCX, arg(0));
            }
            None => a.lea(Reg::RDX, arg(0)),
        }
        self.call(e);
        if let Some(s) = site {
            self.asm.test(Size::D, Reg::RAX, Reg::RAX);
            let l = self.error(s);
            self.asm.jcc(Cc::Ne, l);
        }
        Ok(arg(0))
    }

    // Instructions.

    fn inst(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        match i.op.form() {
            Form::Un | Form::Conv => self.unary(i),
            Form::Bin | Form::Cmp => self.binary(i),
            Form::Sel => self.select(i),
            Form::TrapBin => self.trap_binary(i),
            Form::TrapUn => self.trap_unary(i),
            Form::TrapConv => {
                let from = self.ty(i.ops[0]);
                let v = self.result(i)?;
                let m = self.eval(
                    Entry::EvalUnary,
                    i.op,
                    from,
                    Some(i.ty as u32),
                    &[i.ops[0]],
                    Some(i.ops[1]),
                )?;
                self.define_from(v, m, i.ty)
            }
            Form::EdgeBin => self.edge(i),
            Form::Wide => self.widening(i),
            Form::Scale | Form::TrapScale => self.scale(i),
            Form::Load => self.load(i),
            Form::Store => self.store(i),
            Form::LoadBit => self.load_bit(i),
            Form::Memcpy => self.memcpy(i),
            Form::Memeq => self.memeq(i),
            Form::Prefetch => Ok(()),
            Form::Cas => self.cas(i),
            Form::Atomic => self.atomic(i),
            Form::StrMk => {
                let v = self.result(i)?;
                let m = self.slot_of(v)?;
                self.store_lo(m, i.ops[0])?;
                self.store_lo(hi(m), i.ops[1])
            }
            Form::Br => {
                let b = Block(i.ops[0]);
                self.moves(b, &i.ops[1..])?;
                self.jump(b);
                Ok(())
            }
            Form::Brif => self.brif(i),
            Form::Switch => self.switch(i),
            Form::Ret => {
                let w = i.ops[0];
                if let Some(c) = self.konst(w) {
                    self.asm.mov_imm(Reg::RAX, c as u64);
                } else {
                    let r = self.reg(w)?;
                    if r != Reg::RAX {
                        self.asm.mov(Size::Q, Reg::RAX, r);
                    }
                }
                self.asm.jmp(self.epilogue);
                Ok(())
            }
            Form::Trap => {
                let l = self.error(i.ops[0]);
                self.asm.jmp(l);
                Ok(())
            }
            Form::Rtcall => self.rtcall(i),
            Form::Vcall => self.vcall(i),
            Form::Guard => {
                let l = self.exit(status::make(status::DEOPT, u64::from(i.ops[1])));
                match self.konst(i.ops[0]) {
                    Some(c) if c & 1 == 0 => self.asm.jmp(l),
                    Some(_) => {}
                    None => {
                        let c = self.reg(i.ops[0])?;
                        self.asm.test_imm(Size::B, c, 1);
                        self.asm.jcc(Cc::E, l);
                    }
                }
                Ok(())
            }
            Form::Poll => {
                let n = i.ops[0].max(1);
                let (stub, back) = (self.asm.label(), self.asm.label());
                let a = &mut self.asm;
                a.alu_mem_imm(Alu::Add, Size::D, slot(POLLS), 1);
                a.alu_mem_imm(Alu::Cmp, Size::D, slot(POLLS), n as i32);
                a.jcc(Cc::Ae, stub);
                a.bind(back);
                let live = POOL
                    .iter()
                    .filter(|r| self.holder[r.0 as usize] != EMPTY)
                    .fold(0, |m, r| m | bit(*r));
                self.polls.push(Poll { stub, back, live });
                Ok(())
            }
            Form::CtrAdd => {
                self.copy_to(T1, i.ops[1])?;
                self.flush();
                self.forget();
                let a = &mut self.asm;
                a.load(Size::Q, Reg::RDI, Mem::at(ST, CTX_OFFSET));
                a.mov_imm(Reg::RSI, u64::from(i.ops[0]));
                a.mov(Size::Q, Reg::RDX, T1);
                self.call(Entry::Count);
                Ok(())
            }
        }
    }

    fn unary(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let a = i.ops[0];
        let from = self.ty(a);
        let to = i.op.result(i.ty);
        let v = self.result(i)?;
        let w = from.bits();
        let same = from == to && narrow(from);
        match i.op {
            Op::Neg | Op::Not if same => {
                let d = self.take(a)?;
                let op = if i.op == Op::Neg { Unary::Neg } else { Unary::Not };
                self.asm.unary(op, Size::Q, d);
                self.norm(to, d);
                self.define(v, d);
            }
            Op::Clz | Op::Ctz if same && w == 1 => {
                let d = self.take(a)?;
                self.asm.alu_imm(Alu::Xor, Size::D, d, 1);
                self.define(v, d);
            }
            Op::Clz if same && self.be.lzcnt => {
                let d = self.take(a)?;
                self.asm.bits(Bits::Lzcnt, Size::Q, d, d);
                if w < 64 {
                    self.asm.alu_imm(Alu::Sub, Size::D, d, 64 - w as i32);
                }
                self.define(v, d);
            }
            Op::Ctz if same && self.be.bmi1 => {
                let d = self.take(a)?;
                if w < 32 {
                    self.asm.alu_imm(Alu::Or, Size::D, d, 1 << w);
                }
                let s = if w == 64 { Size::Q } else { Size::D };
                self.asm.bits(Bits::Tzcnt, s, d, d);
                self.define(v, d);
            }
            Op::Popcnt if same && (w == 1 || self.be.popcnt) => {
                let d = self.take(a)?;
                if w > 1 {
                    self.asm.bits(Bits::Popcnt, Size::Q, d, d);
                }
                self.define(v, d);
            }
            Op::Bswap if same => {
                let d = self.take(a)?;
                match w {
                    1 => self.asm.mov_imm(d, 0),
                    8 => {}
                    16 => self.asm.shift_imm(Shift::Rol, Size::W, d, 8),
                    32 => self.asm.bswap(Size::D, d),
                    _ => self.asm.bswap(Size::Q, d),
                }
                self.define(v, d);
            }
            Op::Sext | Op::Zext | Op::Trunc | Op::Bitcast if w > 0 && to.bits() > 0 => {
                self.resize(v, a, from, to, i.op == Op::Sext)?;
            }
            Op::Sitof | Op::Uitof
                if narrow(from) && to.is_float() && (i.op == Op::Sitof || w < 64) =>
            {
                let d = self.take(a)?;
                if i.op == Op::Sitof {
                    self.sext(from, d);
                }
                self.asm.int_to_float(Prec::D, X0, d);
                if to == Ty::F32 {
                    self.asm.float_to_float(Prec::S, X0, X0);
                }
                self.asm.from_xmm(prec(to), d, X0);
                self.define(v, d);
            }
            Op::Fext if from == Ty::F32 && to == Ty::F64 => {
                let r = self.reg(a)?;
                self.asm.to_xmm(Prec::S, X0, r);
                self.asm.float_to_float(Prec::D, X0, X0);
                let d = self.fresh()?;
                self.asm.from_xmm(Prec::D, d, X0);
                self.define(v, d);
            }
            Op::Ftrunc if from == Ty::F64 && to.is_float() => {
                let d = self.take(a)?;
                if to == Ty::F32 {
                    self.asm.to_xmm(Prec::D, X0, d);
                    self.asm.float_to_float(Prec::S, X0, X0);
                    self.asm.from_xmm(Prec::S, d, X0);
                }
                self.define(v, d);
            }
            Op::Fneg | Op::Fabs if from == Ty::F64 && to == Ty::F64 => {
                let d = self.take(a)?;
                let (op, m) = if i.op == Op::Fneg {
                    (Alu::Xor, 1u64 << 63)
                } else {
                    (Alu::And, !(1u64 << 63))
                };
                self.asm.mov_imm(T1, m);
                self.asm.alu(op, Size::Q, d, T1);
                self.define(v, d);
            }
            Op::Fsqrt if from == to && from.is_float() => {
                let p = prec(from);
                let d = self.take(a)?;
                self.asm.to_xmm(p, X0, d);
                self.asm.farith(Fop::Sqrt, p, X0, X0);
                self.asm.from_xmm(p, d, X0);
                self.define(v, d);
            }
            Op::StrLen | Op::StrW0 | Op::StrW1 | Op::StrPtr | Op::StrInl if w == 128 => {
                let m = self.mem(a, 0)?;
                let d = self.fresh()?;
                match i.op {
                    Op::StrLen => self.asm.load(Size::D, d, m),
                    Op::StrW0 => self.asm.load(Size::Q, d, m),
                    Op::StrW1 | Op::StrPtr => self.asm.load(Size::Q, d, hi(m)),
                    _ => {
                        self.asm.load(Size::D, d, m);
                        self.asm.alu_imm(Alu::Cmp, Size::D, d, 12);
                        self.asm.mov_imm(d, 0);
                        self.asm.setcc(Cc::Be, d);
                    }
                }
                self.define(v, d);
            }
            _ => {
                let m = self.eval(Entry::EvalUnary, i.op, from, Some(to as u32), &[a], None)?;
                self.define_from(v, m, to)?;
            }
        }
        Ok(())
    }

    /// `sext`, `zext`, `trunc` and `bitcast`, which all move bits between widths.
    fn resize(&mut self, v: Val, a: u32, from: Ty, to: Ty, signed: bool) -> Result<(), Error> {
        match (wide(from), wide(to)) {
            (false, false) => {
                let d = self.take(a)?;
                if signed {
                    self.sext(from, d);
                }
                self.norm(to, d);
                self.define(v, d);
            }
            (false, true) => {
                let d = self.take(a)?;
                let m = self.slot_of(v)?;
                if signed {
                    self.sext(from, d);
                }
                self.asm.store(Size::Q, m, d);
                if signed {
                    self.asm.shift_imm(Shift::Sar, Size::Q, d, 63);
                    self.asm.store(Size::Q, hi(m), d);
                } else {
                    self.asm.store_imm(Size::Q, hi(m), 0);
                }
            }
            (true, false) => {
                let m = self.mem(a, 0)?;
                let d = self.fresh()?;
                self.asm.load(Size::Q, d, m);
                self.norm(to, d);
                self.define(v, d);
            }
            (true, true) => {
                let m = self.mem(a, 0)?;
                self.define_wide(v, m)?;
            }
        }
        Ok(())
    }

    fn binary(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, b) = (i.ops[0], i.ops[1]);
        let ty = i.ty;
        let to = i.op.result(ty);
        let v = self.result(i)?;
        let n = narrow(ty);
        let w = ty.bits();
        match i.op {
            Op::Add | Op::Sub | Op::Mul | Op::And | Op::Or | Op::Xor if n => {
                let d = self.take(a)?;
                self.arith(i.op, Size::Q, d, b, ty)?;
                self.norm(ty, d);
                self.define(v, d);
            }
            Op::Add | Op::Sub | Op::And | Op::Or | Op::Xor if w == 128 => {
                let m = self.slot_of(v)?;
                self.wide_arith(i.op, m, a, b, None)?;
            }
            Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if n => {
                self.shift(i.op, v, a, b, ty)?;
            }
            Op::Fadd | Op::Fsub | Op::Fmul | Op::Fdiv if ty.is_float() => {
                let p = prec(ty);
                let ra = self.reg(a)?;
                self.asm.to_xmm(p, X0, ra);
                let rb = self.reg(b)?;
                self.asm.to_xmm(p, X1, rb);
                let op = match i.op {
                    Op::Fadd => Fop::Add,
                    Op::Fsub => Fop::Sub,
                    Op::Fmul => Fop::Mul,
                    _ => Fop::Div,
                };
                self.asm.farith(op, p, X0, X1);
                let d = self.fresh()?;
                self.asm.from_xmm(p, d, X0);
                self.define(v, d);
            }
            Op::FminTot | Op::FmaxTot if ty.is_float() => {
                let p = prec(ty);
                let ra = self.reg(a)?;
                let rb = self.reg(b)?;
                let (d, other) =
                    if i.op == Op::FminTot { (self.take(b)?, ra) } else { (self.take(a)?, rb) };
                self.asm.to_xmm(p, X0, ra);
                self.asm.to_xmm(p, X1, rb);
                self.total_lt(p, X0, X1, T0);
                self.asm.test(Size::D, T0, T0);
                self.asm.cmov(Cc::Ne, Size::Q, d, other);
                self.define(v, d);
            }
            Op::FcmpEq | Op::FcmpLt | Op::FcmpLe if ty.is_float() => {
                let p = prec(ty);
                let ra = self.reg(a)?;
                self.asm.to_xmm(p, X0, ra);
                let rb = self.reg(b)?;
                self.asm.to_xmm(p, X1, rb);
                let d = self.fresh()?;
                match i.op {
                    Op::FcmpLt => self.total_lt(p, X0, X1, d),
                    Op::FcmpLe => {
                        self.total_lt(p, X1, X0, d);
                        self.asm.alu_imm(Alu::Xor, Size::D, d, 1);
                    }
                    _ => self.total_eq(p, X0, X1, d),
                }
                self.define(v, d);
            }
            Op::IcmpEq
            | Op::IcmpNe
            | Op::IcmpSlt
            | Op::IcmpSle
            | Op::IcmpSgt
            | Op::IcmpSge
            | Op::IcmpUlt
            | Op::IcmpUle
            | Op::IcmpUgt
            | Op::IcmpUge
                if n =>
            {
                let cc = match i.op {
                    Op::IcmpEq => Cc::E,
                    Op::IcmpNe => Cc::Ne,
                    Op::IcmpSlt => Cc::L,
                    Op::IcmpSle => Cc::Le,
                    Op::IcmpSgt => Cc::G,
                    Op::IcmpSge => Cc::Ge,
                    Op::IcmpUlt => Cc::B,
                    Op::IcmpUle => Cc::Be,
                    Op::IcmpUgt => Cc::A,
                    _ => Cc::Ae,
                };
                let signed = matches!(i.op, Op::IcmpSlt | Op::IcmpSle | Op::IcmpSgt | Op::IcmpSge);
                let d = self.fresh()?;
                if signed && w < 64 {
                    self.copy_to(T0, a)?;
                    self.sext(ty, T0);
                    if let Some(k) = self.imm(b, ty) {
                        self.asm.alu_imm(Alu::Cmp, Size::Q, T0, k);
                    } else {
                        self.copy_to(T1, b)?;
                        self.sext(ty, T1);
                        self.asm.alu(Alu::Cmp, Size::Q, T0, T1);
                    }
                } else {
                    let ra = self.reg(a)?;
                    if let Some(k) = self.imm64(b) {
                        self.asm.alu_imm(Alu::Cmp, Size::Q, ra, k);
                    } else {
                        let rb = self.reg(b)?;
                        self.asm.alu(Alu::Cmp, Size::Q, ra, rb);
                    }
                }
                self.asm.mov_imm(d, 0);
                self.asm.setcc(cc, d);
                self.define(v, d);
            }
            Op::Crc32c if self.be.sse42 && (w == 32 || w == 64) && !ty.is_float() => {
                let d = self.take(a)?;
                let rb = self.reg(b)?;
                self.asm.crc32(d, rb);
                self.define(v, d);
            }
            _ => {
                let m = self.eval(Entry::EvalBinary, i.op, ty, None, &[a, b], None)?;
                self.define_from(v, m, to)?;
            }
        }
        Ok(())
    }

    /// `d = d op b` at `s`, for the plain and the checked arithmetic.
    fn arith(&mut self, op: Op, s: Size, d: Reg, b: u32, ty: Ty) -> Result<(), Error> {
        let alu = match op {
            Op::Add | Op::SaddT | Op::UaddT | Op::SaddOv => Alu::Add,
            Op::Sub | Op::SsubT | Op::UsubT | Op::SsubOv => Alu::Sub,
            Op::And => Alu::And,
            Op::Or => Alu::Or,
            Op::Xor => Alu::Xor,
            _ => {
                if let Some(k) = self.imm(b, ty) {
                    self.asm.imul_imm(s, d, d, k);
                } else {
                    let rb = self.reg(b)?;
                    self.asm.imul(s, d, rb);
                }
                return Ok(());
            }
        };
        if let Some(k) = self.imm(b, ty) {
            self.asm.alu_imm(alu, s, d, k);
        } else {
            let rb = self.reg(b)?;
            self.asm.alu(alu, s, d, rb);
        }
        Ok(())
    }

    /// A 128 bit add, subtract or bitwise operation into `m`. With a condition, it is checked
    /// and jumps to `trap` when the condition holds after the high half.
    fn wide_arith(
        &mut self,
        op: Op,
        m: Mem,
        a: u32,
        b: u32,
        trap: Option<(Cc, Label)>,
    ) -> Result<(), Error> {
        let (lo, high) = match op {
            Op::Add | Op::SaddT | Op::UaddT | Op::SaddOv => (Alu::Add, Alu::Adc),
            Op::Sub | Op::SsubT | Op::UsubT | Op::SsubOv => (Alu::Sub, Alu::Sbb),
            Op::And => (Alu::And, Alu::And),
            Op::Or => (Alu::Or, Alu::Or),
            _ => (Alu::Xor, Alu::Xor),
        };
        let ma = self.mem(a, 0)?;
        let mb = self.mem(b, 1)?;
        let asm = &mut self.asm;
        asm.load(Size::Q, T0, ma);
        asm.load(Size::Q, T1, hi(ma));
        asm.alu_load(lo, Size::Q, T0, mb);
        asm.alu_load(high, Size::Q, T1, hi(mb));
        if let Some((cc, l)) = trap {
            asm.jcc(cc, l);
        }
        asm.store(Size::Q, m, T0);
        asm.store(Size::Q, hi(m), T1);
        Ok(())
    }

    fn shift(&mut self, op: Op, v: Val, a: u32, b: u32, ty: Ty) -> Result<(), Error> {
        let w = ty.bits();
        if w == 1 {
            let d = self.take(a)?;
            self.define(v, d);
            return Ok(());
        }
        let rotate = matches!(op, Op::Rotl | Op::Rotr);
        let (kind, s) = match op {
            Op::Shl => (Shift::Shl, Size::Q),
            Op::Lshr => (Shift::Shr, Size::Q),
            Op::Ashr => (Shift::Sar, Size::Q),
            Op::Rotl => (Shift::Rol, size(ty)),
            _ => (Shift::Ror, size(ty)),
        };
        if let Some(c) = self.konst(b) {
            let k = (c as u32) % w;
            let d = self.take(a)?;
            if op == Op::Ashr {
                self.sext(ty, d);
            }
            if k > 0 {
                self.asm.shift_imm(kind, s, d, k as u8);
            }
            if !rotate {
                self.norm(ty, d);
            }
            self.define(v, d);
            return Ok(());
        }
        let rb = self.reg(b)?;
        self.asm.mov(Size::D, T1, rb);
        self.claim(Reg::RCX);
        let d = self.take(a)?;
        if !rotate && w < 64 {
            self.asm.alu_imm(Alu::And, Size::D, T1, w as i32 - 1);
        }
        self.asm.mov(Size::D, Reg::RCX, T1);
        if op == Op::Ashr {
            self.sext(ty, d);
        }
        self.asm.shift_cl(kind, s, d);
        if !rotate {
            self.norm(ty, d);
        }
        self.define(v, d);
        Ok(())
    }

    /// `out = a < b` in the total order: NaN is above every number and equal to itself.
    fn total_lt(&mut self, p: Prec, a: Xmm, b: Xmm, out: Reg) {
        let done = self.asm.label();
        let asm = &mut self.asm;
        asm.mov_imm(out, 0);
        asm.ucomis(p, b, a);
        asm.setcc(Cc::A, out);
        asm.ucomis(p, b, b);
        asm.jcc(Cc::Np, done);
        asm.ucomis(p, a, a);
        asm.setcc(Cc::Np, out);
        asm.bind(done);
    }

    /// `out = a == b` in the total order.
    fn total_eq(&mut self, p: Prec, a: Xmm, b: Xmm, out: Reg) {
        let (nan, done) = (self.asm.label(), self.asm.label());
        let asm = &mut self.asm;
        asm.mov_imm(out, 0);
        asm.ucomis(p, a, b);
        asm.jcc(Cc::P, nan);
        asm.setcc(Cc::E, out);
        asm.jmp(done);
        asm.bind(nan);
        asm.ucomis(p, a, a);
        asm.jcc(Cc::Np, done);
        asm.ucomis(p, b, b);
        asm.setcc(Cc::P, out);
        asm.bind(done);
    }

    fn select(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (c, a, b) = (i.ops[0], i.ops[1], i.ops[2]);
        let v = self.result(i)?;
        if let Some(k) = self.konst(c) {
            let pick = if k & 1 != 0 { a } else { b };
            if wide(i.ty) {
                let m = self.mem(pick, 0)?;
                return self.define_wide(v, m);
            }
            let d = self.take(pick)?;
            self.define(v, d);
            return Ok(());
        }
        if wide(i.ty) {
            let ma = self.mem(a, 0)?;
            let mb = self.mem(b, 1)?;
            let rc = self.reg(c)?;
            let (x, y) = (self.fresh()?, self.fresh()?);
            let m = self.slot_of(v)?;
            let asm = &mut self.asm;
            asm.load(Size::Q, T0, mb);
            asm.load(Size::Q, T1, hi(mb));
            asm.load(Size::Q, x, ma);
            asm.load(Size::Q, y, hi(ma));
            asm.test_imm(Size::B, rc, 1);
            asm.cmov(Cc::Ne, Size::Q, T0, x);
            asm.cmov(Cc::Ne, Size::Q, T1, y);
            asm.store(Size::Q, m, T0);
            asm.store(Size::Q, hi(m), T1);
            return Ok(());
        }
        let rc = self.reg(c)?;
        let ra = self.reg(a)?;
        let d = self.take(b)?;
        self.asm.test_imm(Size::B, rc, 1);
        self.asm.cmov(Cc::Ne, Size::Q, d, ra);
        self.define(v, d);
        Ok(())
    }

    fn trap_binary(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, b, site) = (i.ops[0], i.ops[1], i.ops[2]);
        let ty = i.ty;
        let v = self.result(i)?;
        let w = ty.bits();
        let s = size(ty);
        let inline = narrow(ty) && ty != Ty::I1 && !ty.is_float();
        match i.op {
            Op::SaddT | Op::SsubT | Op::UaddT | Op::UsubT if inline => {
                let d = self.take(a)?;
                self.arith(i.op, s, d, b, ty)?;
                let cc = if matches!(i.op, Op::SaddT | Op::SsubT) { Cc::O } else { Cc::B };
                let l = self.error(site);
                self.asm.jcc(cc, l);
                self.define(v, d);
            }
            Op::SaddT | Op::SsubT | Op::UaddT | Op::UsubT if w == 128 => {
                let cc = if matches!(i.op, Op::SaddT | Op::SsubT) { Cc::O } else { Cc::B };
                let l = self.error(site);
                let m = self.slot_of(v)?;
                self.wide_arith(i.op, m, a, b, Some((cc, l)))?;
            }
            Op::SmulT if inline && w >= 16 => {
                let d = self.take(a)?;
                self.arith(i.op, s, d, b, ty)?;
                let l = self.error(site);
                self.asm.jcc(Cc::O, l);
                self.define(v, d);
            }
            Op::UmulT if inline && w < 64 => {
                // Both operands are zero extended, so the product is exact in 64 bits and an
                // immediate has to be the operand's value, not its sign extension.
                let d = self.take(a)?;
                if let Some(k) = self.imm64(b) {
                    self.asm.imul_imm(Size::Q, d, d, k);
                } else {
                    let rb = self.reg(b)?;
                    self.asm.imul(Size::Q, d, rb);
                }
                self.asm.mov(Size::Q, T1, d);
                self.asm.shift_imm(Shift::Shr, Size::Q, T1, w as u8);
                let l = self.error(site);
                self.asm.jcc(Cc::Ne, l);
                self.define(v, d);
            }
            Op::SdivT | Op::SremT | Op::UdivT | Op::UremT if inline => {
                let signed = matches!(i.op, Op::SdivT | Op::SremT);
                self.copy_to(T0, a)?;
                self.copy_to(T1, b)?;
                if signed {
                    self.sext(ty, T0);
                    self.sext(ty, T1);
                }
                self.claim(Reg::RAX);
                self.claim(Reg::RDX);
                let err = self.error(site);
                let asm = &mut self.asm;
                asm.test(Size::Q, T1, T1);
                asm.jcc(Cc::E, err);
                let quotient = matches!(i.op, Op::SdivT | Op::UdivT);
                if signed {
                    let (divide, done) = (asm.label(), asm.label());
                    asm.alu_imm(Alu::Cmp, Size::Q, T1, -1);
                    asm.jcc(Cc::Ne, divide);
                    if quotient {
                        if w < 64 {
                            asm.alu_imm(Alu::Cmp, Size::Q, T0, (-(1i64 << (w - 1))) as i32);
                        } else {
                            asm.mov_imm(Reg::RAX, 1 << 63);
                            asm.alu(Alu::Cmp, Size::Q, T0, Reg::RAX);
                        }
                        asm.jcc(Cc::E, err);
                    } else {
                        asm.mov_imm(Reg::RDX, 0);
                        asm.jmp(done);
                    }
                    asm.bind(divide);
                    asm.mov(Size::Q, Reg::RAX, T0);
                    asm.sign_extend_ax(Size::Q);
                    asm.unary(Unary::Idiv, Size::Q, T1);
                    asm.bind(done);
                } else {
                    asm.mov(Size::Q, Reg::RAX, T0);
                    asm.alu(Alu::Xor, Size::D, Reg::RDX, Reg::RDX);
                    asm.unary(Unary::Div, Size::Q, T1);
                }
                let r = if quotient { Reg::RAX } else { Reg::RDX };
                if signed {
                    self.norm(ty, r);
                }
                self.define(v, r);
            }
            _ => {
                let m = self.eval(Entry::EvalBinary, i.op, ty, None, &[a, b], Some(site))?;
                self.define_from(v, m, ty)?;
            }
        }
        Ok(())
    }

    fn trap_unary(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, site) = (i.ops[0], i.ops[1]);
        let ty = i.ty;
        let v = self.result(i)?;
        if i.op == Op::SnegT && narrow(ty) && ty != Ty::I1 && !ty.is_float() {
            let d = self.take(a)?;
            match size(ty) {
                Size::Q => {
                    self.asm.mov_imm(T1, 1 << 63);
                    self.asm.alu(Alu::Cmp, Size::Q, d, T1);
                }
                s => {
                    let min = -(1i64 << (ty.bits() - 1));
                    self.asm.alu_imm(Alu::Cmp, s, d, min as i32);
                }
            }
            let l = self.error(site);
            self.asm.jcc(Cc::E, l);
            self.asm.unary(Unary::Neg, Size::Q, d);
            self.norm(ty, d);
            self.define(v, d);
            return Ok(());
        }
        let m = self.eval(Entry::EvalUnary, i.op, ty, Some(ty as u32), &[a], Some(site))?;
        self.define_from(v, m, ty)
    }

    fn edge(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, b) = (i.ops[0], i.ops[1]);
        let (ok, ovf) = (Block(i.ops[2]), Block(i.ops[3]));
        let ty = i.ty;
        let p = *self.f.blocks[ok.index()]
            .params
            .first()
            .ok_or_else(|| self.fail(format!("{} into a block with no parameter", i.op.name())))?;
        let to = self.labels[ovf.index()];
        let w = ty.bits();
        if ty.is_intlike() && narrow(ty) && ty != Ty::I1 && !(i.op == Op::SmulOv && w < 16) {
            let d = self.take(a)?;
            self.arith(i.op, size(ty), d, b, ty)?;
            self.asm.jcc(Cc::O, to);
            self.write(self.home[p.index()], ty, Src::Reg(d))?;
        } else if w == 128 && matches!(i.op, Op::SaddOv | Op::SsubOv) {
            let m = self.slot_of(p)?;
            self.wide_arith(i.op, m, a, b, Some((Cc::O, to)))?;
        } else {
            let m = self.eval(Entry::EvalBinary, i.op, ty, None, &[a, b], None)?;
            self.asm.test(Size::D, Reg::RAX, Reg::RAX);
            self.asm.jcc(Cc::Ne, to);
            self.write(self.home[p.index()], ty, Src::Mem(m))?;
        }
        self.jump(ok);
        Ok(())
    }

    fn widening(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, b) = (i.ops[0], i.ops[1]);
        let ty = i.ty;
        let v = self.result(i)?;
        if !narrow(ty) {
            let m = self.eval(Entry::EvalBinary, i.op, ty, None, &[a, b], None)?;
            return self.define_wide(v, m);
        }
        let signed = i.op == Op::Smulw;
        self.copy_to(T0, a)?;
        self.copy_to(T1, b)?;
        if signed {
            self.sext(ty, T0);
            self.sext(ty, T1);
        }
        self.claim(Reg::RAX);
        self.claim(Reg::RDX);
        let m = self.slot_of(v)?;
        let asm = &mut self.asm;
        asm.mov(Size::Q, Reg::RAX, T0);
        asm.unary(if signed { Unary::Imul } else { Unary::Mul }, Size::Q, T1);
        asm.store(Size::Q, m, Reg::RAX);
        asm.store(Size::Q, hi(m), Reg::RDX);
        Ok(())
    }

    fn scale(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (a, k) = (i.ops[0], i.ops[1]);
        let ty = i.ty;
        let v = self.result(i)?;
        let site = (i.op.form() == Form::TrapScale).then(|| i.ops[2]);
        if let Some(site) = site {
            let w = ty.bits();
            let limit = if w == 16 { 4 } else { 9 };
            if matches!(w, 16 | 32 | 64) && ty.is_int() && k <= limit {
                let p = 10i32.pow(k);
                let d = self.take(a)?;
                self.asm.imul_imm(size(ty), d, d, p);
                let l = self.error(site);
                self.asm.jcc(Cc::O, l);
                self.define(v, d);
                return Ok(());
            }
        }
        let m = self.eval(Entry::EvalScale, i.op, ty, Some(k), &[a], site)?;
        self.define_from(v, m, ty)
    }

    /// The address of a load or a store.
    fn addr(&mut self, o: &[u32]) -> Result<Mem, Error> {
        let base = self.reg(o[0])?;
        let (idx, scale, disp) = (o[1], o[2], o[3] as i32);
        if idx == Val::NONE.0 || scale == 0 {
            return Ok(Mem::at(base, disp));
        }
        if let Some(c) = self.konst(idx) {
            let off =
                i64::from(disp).wrapping_add((c as u64 as i64).wrapping_mul(i64::from(scale)));
            if let Ok(off) = i32::try_from(off) {
                return Ok(Mem::at(base, off));
            }
        }
        let ri = self.reg(idx)?;
        Ok(match scale {
            1 | 2 | 4 | 8 => Mem::indexed(base, ri, scale as u8, disp),
            _ => {
                if let Ok(k) = i32::try_from(scale) {
                    self.asm.imul_imm(Size::Q, T1, ri, k);
                } else {
                    self.asm.mov_imm(T1, u64::from(scale));
                    self.asm.imul(Size::Q, T1, ri);
                }
                Mem::indexed(base, T1, 1, disp)
            }
        })
    }

    fn load(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let v = self.result(i)?;
        let ty = i.ty;
        let to = i.op.result(ty);
        let m = self.addr(i.ops)?;
        if wide(ty) {
            return self.define_wide(v, m);
        }
        let d = self.fresh()?;
        self.asm.load(size(ty), d, m);
        if ty == Ty::I1 {
            self.asm.alu_imm(Alu::And, Size::D, d, 1);
        }
        if wide(to) {
            let s = self.slot_of(v)?;
            self.asm.store(Size::Q, s, d);
            self.asm.store_imm(Size::Q, hi(s), 0);
        } else {
            self.define(v, d);
        }
        Ok(())
    }

    fn store(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let ty = i.ty;
        let w = i.ops[4];
        let vt = self.ty(w);
        let m = self.addr(i.ops)?;
        if wide(ty) {
            if wide(vt) {
                let s = self.mem(w, 0)?;
                self.asm.vload(X15, s);
                self.asm.vstore(m, X15);
            } else {
                self.store_lo(m, w)?;
                self.asm.store_imm(Size::Q, hi(m), 0);
            }
            return Ok(());
        }
        let s = size(ty);
        if let Some(c) = self.konst(w) {
            let c = if ty == Ty::I1 { c & 1 } else { c };
            self.store_const(s, m, c as u64);
        } else if wide(vt) || (ty == Ty::I1 && vt != Ty::I1) {
            let r = self.reg(w)?;
            self.asm.mov(Size::Q, T0, r);
            if ty == Ty::I1 {
                self.asm.alu_imm(Alu::And, Size::D, T0, 1);
            }
            self.asm.store(s, m, T0);
        } else {
            let r = self.reg(w)?;
            self.asm.store(s, m, r);
        }
        Ok(())
    }

    fn load_bit(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let v = self.result(i)?;
        let base = self.reg(i.ops[0])?;
        let idx = self.reg(i.ops[1])?;
        let d = self.fresh()?;
        let asm = &mut self.asm;
        asm.mov(Size::Q, T1, idx);
        asm.shift_imm(Shift::Shr, Size::Q, T1, 3);
        asm.load(Size::B, d, Mem::indexed(base, T1, 1, 0));
        asm.mov(Size::D, T1, idx);
        asm.alu_imm(Alu::And, Size::D, T1, 7);
        asm.bt(Size::D, d, T1);
        asm.mov_imm(d, 0);
        asm.setcc(Cc::B, d);
        self.define(v, d);
        Ok(())
    }

    fn memcpy(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let n = i.ops[2];
        if n > 64 {
            return Err(self.fail(format!("memcpy of {n} bytes")));
        }
        let dst = self.reg(i.ops[0])?;
        let src = self.reg(i.ops[1])?;
        // Every piece is read before any is written, so an overlapping copy is `ptr::copy`. A
        // piece is in a scratch register when it is `Ok` and in an XMM register when it is `Err`.
        let mut pieces = Vec::with_capacity(8);
        let mut off = 0i32;
        let mut left = n;
        while left >= 16 {
            pieces.push((off, 16));
            off += 16;
            left -= 16;
        }
        for k in [8, 4, 2, 1] {
            if left >= k {
                pieces.push((off, k));
                off += k as i32;
                left -= k;
            }
        }
        let mut xmm = 0u8;
        let mut places = Vec::with_capacity(pieces.len());
        for &(off, k) in &pieces {
            let at = Mem::at(src, off);
            let place = match k {
                2 => {
                    self.asm.load(Size::W, T0, at);
                    Ok(T0)
                }
                1 => {
                    self.asm.load(Size::B, T1, at);
                    Ok(T1)
                }
                _ => {
                    let x = Xmm(xmm);
                    xmm += 1;
                    match k {
                        16 => self.asm.vload(x, at),
                        8 => self.asm.fload(Prec::D, x, at),
                        _ => self.asm.fload(Prec::S, x, at),
                    }
                    Err(x)
                }
            };
            places.push(place);
        }
        for (&(off, k), place) in pieces.iter().zip(places) {
            let at = Mem::at(dst, off);
            match place {
                Ok(r) => self.asm.store(if k == 2 { Size::W } else { Size::B }, at, r),
                Err(x) if k == 16 => self.asm.vstore(at, x),
                Err(x) => self.asm.fstore(if k == 8 { Prec::D } else { Prec::S }, at, x),
            }
        }
        Ok(())
    }

    fn memeq(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let n = i.ops[2];
        if n > 256 {
            return Err(self.fail(format!("memeq of {n} bytes")));
        }
        let v = self.result(i)?;
        let a = self.reg(i.ops[0])?;
        let b = self.reg(i.ops[1])?;
        let d = self.fresh()?;
        if n == 0 {
            self.asm.mov_imm(d, 1);
            self.define(v, d);
            return Ok(());
        }
        let asm = &mut self.asm;
        let (mut off, mut left, mut first) = (0i32, n, true);
        while left > 0 {
            let (s, k) = match left {
                8.. => (Size::Q, 8),
                4..=7 => (Size::D, 4),
                2..=3 => (Size::W, 2),
                _ => (Size::B, 1),
            };
            let r = if first { T0 } else { T1 };
            asm.load(s, r, Mem::at(a, off));
            asm.alu_load(Alu::Xor, s, r, Mem::at(b, off));
            if !first {
                asm.alu(Alu::Or, Size::Q, T0, T1);
            }
            first = false;
            off += k as i32;
            left -= k;
        }
        asm.test(Size::Q, T0, T0);
        asm.mov_imm(d, 0);
        asm.setcc(Cc::E, d);
        self.define(v, d);
        Ok(())
    }

    fn cas(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (at, old, new) = (i.ops[0], i.ops[1], i.ops[2]);
        let ty = i.ty;
        let v = self.result(i)?;
        let ra = self.reg(at)?;
        let skip = self.asm.label();
        if wide(ty) {
            let mo = self.mem(old, 0)?;
            let mn = self.mem(new, 1)?;
            let d = self.fresh()?;
            let m = Mem::at(ra, 0);
            let asm = &mut self.asm;
            asm.load(Size::Q, T0, m);
            asm.alu_load(Alu::Xor, Size::Q, T0, mo);
            asm.load(Size::Q, T1, hi(m));
            asm.alu_load(Alu::Xor, Size::Q, T1, hi(mo));
            asm.alu(Alu::Or, Size::Q, T0, T1);
            asm.mov_imm(d, 0);
            asm.setcc(Cc::E, d);
            asm.jcc(Cc::Ne, skip);
            asm.vload(X15, mn);
            asm.vstore(m, X15);
            asm.bind(skip);
            self.define(v, d);
            return Ok(());
        }
        let s = size(ty);
        let rn = self.reg(new)?;
        let d = self.fresh()?;
        self.asm.load(s, T0, Mem::at(ra, 0));
        if ty == Ty::I1 {
            self.asm.alu_imm(Alu::And, Size::D, T0, 1);
        }
        self.copy_to(T1, old)?;
        self.norm(ty, T1);
        let asm = &mut self.asm;
        asm.alu(Alu::Cmp, Size::Q, T0, T1);
        asm.mov_imm(d, 0);
        asm.setcc(Cc::E, d);
        asm.jcc(Cc::Ne, skip);
        if ty == Ty::I1 {
            asm.mov(Size::D, T0, rn);
            asm.alu_imm(Alu::And, Size::D, T0, 1);
            asm.store(Size::B, Mem::at(ra, 0), T0);
        } else {
            asm.store(s, Mem::at(ra, 0), rn);
        }
        asm.bind(skip);
        self.define(v, d);
        Ok(())
    }

    fn atomic(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let (at, w) = (i.ops[0], i.ops[1]);
        let ty = i.ty;
        let v = self.result(i)?;
        let ra = self.reg(at)?;
        let m = Mem::at(ra, 0);
        if wide(ty) {
            let d = self.slot_of(v)?;
            self.asm.vload(X15, m);
            self.asm.vstore(d, X15);
            if wide(self.ty(w)) {
                let mw = self.mem(w, 0)?;
                let asm = &mut self.asm;
                asm.load(Size::Q, T0, mw);
                asm.load(Size::Q, T1, hi(mw));
                asm.alu_store(Alu::Add, Size::Q, m, T0);
                asm.alu_store(Alu::Adc, Size::Q, hi(m), T1);
            } else {
                self.copy_to(T0, w)?;
                self.asm.alu_store(Alu::Add, Size::Q, m, T0);
                self.asm.alu_mem_imm(Alu::Adc, Size::Q, hi(m), 0);
            }
            return Ok(());
        }
        let s = size(ty);
        let rw = self.reg(w)?;
        let d = self.fresh()?;
        let asm = &mut self.asm;
        asm.load(s, d, m);
        if ty == Ty::I1 {
            asm.alu_imm(Alu::And, Size::D, d, 1);
        }
        asm.mov(Size::Q, T1, d);
        asm.alu(Alu::Add, Size::Q, T1, rw);
        if ty == Ty::I1 {
            asm.alu_imm(Alu::And, Size::D, T1, 1);
        }
        asm.store(s, m, T1);
        self.define(v, d);
        Ok(())
    }

    // Control flow.

    /// Whether the branch to `b` passing `args` writes any parameter.
    fn has_moves(&self, b: Block, args: &[u32]) -> bool {
        self.f.blocks[b.index()].params.iter().zip(args).any(|(p, a)| p.0 != *a)
    }

    /// Writes `src` to a home, as a value of type `t`.
    fn write(&mut self, to: Home, t: Ty, src: Src) -> Result<(), Error> {
        let asm = &mut self.asm;
        match (to, src) {
            (Home::Reg(p), Src::Reg(r)) => {
                if p != r {
                    asm.mov(Size::Q, p, r);
                }
            }
            (Home::Reg(p), Src::Mem(m)) => asm.load(Size::Q, p, m),
            (Home::Reg(p), Src::Imm(c)) => asm.mov_imm(p, c as u64),
            (Home::Slot(d), _) if wide(t) => match src {
                Src::Mem(m) => {
                    asm.vload(X15, m);
                    asm.vstore(slot(d), X15);
                }
                Src::Imm(c) => {
                    self.store_const(Size::Q, slot(d), c as u64);
                    self.store_const(Size::Q, hi(slot(d)), (c >> 64) as u64);
                }
                Src::Reg(_) => return Err(self.fail("a wide value in a register")),
            },
            (Home::Slot(d), Src::Reg(r)) => asm.store(Size::Q, slot(d), r),
            (Home::Slot(d), Src::Mem(m)) => {
                asm.load(Size::Q, T0, m);
                asm.store(Size::Q, slot(d), T0);
            }
            (Home::Slot(d), Src::Imm(c)) => self.store_const(Size::Q, slot(d), c as u64),
            (Home::None, _) => {}
        }
        Ok(())
    }

    /// The parallel copy of a branch's arguments into the target's parameters. A source that is
    /// itself a parameter of the target is copied to the call area first, so no write clobbers a
    /// read still to come. Nothing here changes the cache, since a branch is the end of its
    /// block, and a `brif` emits both edges' copies from the same state.
    fn moves(&mut self, b: Block, args: &[u32]) -> Result<(), Error> {
        let f = self.f;
        let params = &f.blocks[b.index()].params;
        if !self.has_moves(b, args) {
            return Ok(());
        }
        self.generation += 1;
        for p in params {
            self.seen[p.index()] = self.generation;
        }
        let mut srcs = Vec::with_capacity(args.len());
        let mut temps = 0;
        for (p, &a) in params.iter().zip(args) {
            let v = Val(a);
            if p.0 == a {
                srcs.push(None);
                continue;
            }
            if !v.is_const() && self.seen[v.index()] == self.generation {
                let m = arg(temps);
                temps += 1;
                if wide(f.ty(v)) {
                    let s = self.slot_of(v)?;
                    self.asm.vload(X15, s);
                    self.asm.vstore(m, X15);
                } else {
                    match self.peek(a)? {
                        Src::Reg(r) => self.asm.store(Size::Q, m, r),
                        Src::Mem(s) => {
                            self.asm.load(Size::Q, T0, s);
                            self.asm.store(Size::Q, m, T0);
                        }
                        Src::Imm(_) => {}
                    }
                }
                srcs.push(Some(Src::Mem(m)));
            } else if wide(f.ty(v)) && !v.is_const() {
                srcs.push(Some(Src::Mem(self.slot_of(v)?)));
            } else {
                srcs.push(Some(self.peek(a)?));
            }
        }
        self.words = self.words.max(temps);
        for (p, src) in params.iter().zip(srcs) {
            if let Some(src) = src {
                self.write(self.home[p.index()], f.ty(*p), src)?;
            }
        }
        Ok(())
    }

    fn brif(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let o = i.ops;
        let n = o[2] as usize;
        let (t, targs) = (Block(o[1]), &o[3..3 + n]);
        let (e, eargs) = (Block(o[3 + n]), &o[4 + n..]);
        if let Some(c) = self.konst(o[0]) {
            let (b, args) = if c & 1 != 0 { (t, targs) } else { (e, eargs) };
            self.moves(b, args)?;
            self.jump(b);
            return Ok(());
        }
        let c = self.reg(o[0])?;
        self.asm.test_imm(Size::B, c, 1);
        let (lt, le) = (self.labels[t.index()], self.labels[e.index()]);
        match (self.has_moves(t, targs), self.has_moves(e, eargs)) {
            (false, false) => {
                if self.next == Some(t) {
                    self.asm.jcc(Cc::E, le);
                } else {
                    self.asm.jcc(Cc::Ne, lt);
                    self.jump(e);
                }
            }
            (false, true) => {
                self.asm.jcc(Cc::Ne, lt);
                self.moves(e, eargs)?;
                self.jump(e);
            }
            (true, false) => {
                self.asm.jcc(Cc::E, le);
                self.moves(t, targs)?;
                self.jump(t);
            }
            (true, true) => {
                let other = self.asm.label();
                self.asm.jcc(Cc::E, other);
                self.moves(t, targs)?;
                self.asm.jmp(lt);
                self.asm.bind(other);
                self.moves(e, eargs)?;
                self.jump(e);
            }
        }
        Ok(())
    }

    fn switch(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let o = i.ops;
        let x = o[0];
        let xt = self.ty(x);
        let n = o[2] as usize;
        let default = Block(o[1]);
        let dargs = &o[3..3 + n];
        let other = self.asm.label();
        let rx = if wide(xt) {
            let m = self.mem(x, 0)?;
            self.asm.alu_mem_imm(Alu::Cmp, Size::Q, hi(m), 0);
            self.asm.jcc(Cc::Ne, other);
            self.asm.load(Size::Q, T0, m);
            T0
        } else {
            self.reg(x)?
        };
        let mut seen = HashSet::new();
        for pair in o[3 + n..].as_chunks::<2>().0 {
            let key = u128::from(pair[0]);
            // The interpreter takes the first case with a key, and a key wider than the value
            // never matches.
            if key & xt.mask() != key || !seen.insert(key) {
                continue;
            }
            if let Ok(k) = i32::try_from(pair[0]) {
                self.asm.alu_imm(Alu::Cmp, Size::Q, rx, k);
            } else {
                self.asm.mov_imm(T1, u64::from(pair[0]));
                self.asm.alu(Alu::Cmp, Size::Q, rx, T1);
            }
            self.asm.jcc(Cc::E, self.labels[pair[1] as usize]);
        }
        self.asm.bind(other);
        self.moves(default, dargs)?;
        self.jump(default);
        Ok(())
    }

    // Calls.

    fn ctx(&mut self) {
        self.asm.load(Size::Q, Reg::RDI, Mem::at(ST, CTX_OFFSET));
    }

    fn rtcall(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let args = &i.ops[1..];
        self.words = self.words.max(args.len());
        for (k, &w) in args.iter().enumerate() {
            self.spill(w, arg(k))?;
        }
        self.flush();
        self.forget();
        self.ctx();
        let asm = &mut self.asm;
        asm.mov_imm(Reg::RSI, u64::from(i.ops[0]));
        asm.lea(Reg::RDX, arg(0));
        asm.mov_imm(Reg::RCX, args.len() as u64);
        asm.lea(Reg::R8, Mem::at(Reg::RSP, 0));
        self.call(Entry::Rtcall);
        self.asm.test(Size::Q, Reg::RAX, Reg::RAX);
        self.asm.jcc(Cc::Ne, self.epilogue);
        if let Some(v) = i.result {
            self.define_from(v, Mem::at(Reg::RSP, 0), i.op.result(i.ty))?;
        }
        Ok(())
    }

    fn vcall(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let bufs = &i.ops[2..];
        self.words = self.words.max(bufs.len());
        for (k, &w) in bufs.iter().enumerate() {
            self.spill(w, arg(k))?;
        }
        self.copy_to(T1, i.ops[1])?;
        self.flush();
        self.forget();
        self.ctx();
        let asm = &mut self.asm;
        asm.mov_imm(Reg::RSI, u64::from(i.ops[0]));
        asm.mov(Size::Q, Reg::RDX, T1);
        asm.lea(Reg::RCX, arg(0));
        asm.mov_imm(Reg::R8, bufs.len() as u64);
        self.call(Entry::Vcall);
        self.asm.test(Size::Q, Reg::RAX, Reg::RAX);
        self.asm.jcc(Cc::Ne, self.epilogue);
        Ok(())
    }
}

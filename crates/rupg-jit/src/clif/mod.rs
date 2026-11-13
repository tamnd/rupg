// Lifted from rudb crates/rudb-qc-clif/src/lib.rs at cd9f9676 (2026-10-05).
// Changes: the module `clif` of rupg-jit behind the `jit` feature, not a crate, so QIR is `crate::ir`, and its imports sort with `crate` first.

//! `clif`, the query compiler's second tier: it lowers a QIR function to machine code through
//! Cranelift, per section 8.4 of `spec/compiler/08-backends.md`.
//!
//! The output is bytes and relocations and nothing else. This crate never maps memory and never
//! calls what it made: `rudb-qc-rt` owns the code arena, and the driver loads a [`Function`] into
//! it and resolves each [`Reloc`] to the address of the runtime [`Entry`] it names. That is what
//! keeps the crate at the rank of the IR, below the runtime, and what makes compiling a pure
//! function of the QIR that can be tested without running anything. It is also why the crate
//! forbids `unsafe`: nothing here touches memory it did not allocate.
//!
//! Only `cranelift-codegen` and `cranelift-frontend` are used, not `cranelift-jit` or
//! `cranelift-module`, because the arena already does what those would do and does it under the
//! epochs of section 8.8.
//!
//! # What has to match
//!
//! `interp` defines what a QIR function computes, and a query switches between the two tiers at
//! morsel boundaries, so every result here has to be the interpreter's to the bit. Most opcodes
//! are one Cranelift instruction with the same meaning. The rest are the ones this module spends
//! its length on:
//!
//! - `i1` is an `i8` holding 0 or 1. The bitwise opcodes keep that by themselves. The others are
//!   written out for `i1` from what [`crate::ir::eval`] computes on one bit.
//! - Checked arithmetic up to 64 bits uses Cranelift's overflow flags. At 128 bits add and
//!   subtract compute the flag from the signs, and multiply and divide call back into
//!   [`crate::ir::eval`] through [`Entry::EvalBinary`], which is the interpreter's own code on
//!   the same bits and so cannot disagree with it.
//! - The float total order that DuckDB sorts by, where NaN equals itself and is the largest, is
//!   two ordered compares and two NaN tests.
//! - `f32` negation and absolute value go through `f64` and back, because the interpreter does,
//!   and that quiets a signalling NaN where the `f32` instruction would not.
//! - `str16` is an `i128` whose low word is the length and the inline bytes.
//! - `guard`, the trap forms and `poll` branch to cold blocks that return the status the
//!   interpreter would have returned.
//! - `crc32c` is the slice by eight table walk over [`Entry::Crc32cTable`], because Cranelift has
//!   no CRC instruction and the table gives the same answer on every target.
//!
//! Anything this module does not lower returns an [`Error`], and the driver runs the function on
//! `interp` instead. A slower query is a better outcome than a wrong one.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;

use crate::ir::entry::{CTX_OFFSET, Entry};
use crate::ir::{Block, Form, Func, Inst, Op, Ty, Val, status};
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64, Imm64};
use cranelift_codegen::ir::{
    self, AbiParam, BlockArg, ExtFuncData, ExternalName, FuncRef, GlobalValueData, InstBuilder,
    MemFlags, Signature, StackSlot, StackSlotData, StackSlotKind, Type, UserExternalName,
    UserFuncName, Value, types,
};
use cranelift_codegen::isa::{self, CallConv, OwnedTargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::{Context, FinalizedRelocTarget, binemit};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Switch};

/// Why a function was not compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The function, or empty when the backend itself could not be made.
    pub func: String,
    /// What went wrong.
    pub reason: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.func.is_empty() {
            write!(f, "clif: {}", self.reason)
        } else {
            write!(f, "clif: {}: {}", self.func, self.reason)
        }
    }
}

impl std::error::Error for Error {}

/// Loads and stores of query memory: they do not trap, and they may be unaligned, since the
/// interpreter reads and writes unaligned.
const MEM: MemFlags = MemFlags::new().with_notrap();

fn fail(func: &str, reason: impl Into<String>) -> Error {
    Error { func: func.to_string(), reason: reason.into() }
}

/// A place in a [`Function`]'s bytes where the absolute address of an [`Entry`] plus `addend` goes,
/// as eight little endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// The byte offset in [`Function::bytes`].
    pub offset: u32,
    /// What the address is the address of.
    pub entry: Entry,
    /// Added to the address.
    pub addend: i64,
}

/// One compiled function: position independent bytes apart from its relocations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// The QIR function's name.
    pub name: String,
    /// The machine code.
    pub bytes: Vec<u8>,
    /// Where the runtime's addresses go.
    pub relocs: Vec<Reloc>,
}

/// A Cranelift target for the machine this process runs on.
pub struct Backend {
    isa: OwnedTargetIsa,
    /// Whether `nearest` is an instruction here. On x86-64 it needs SSE4.1, and without it
    /// Cranelift calls a library function, which this module has no relocation for, so `ftosi.t`
    /// goes through [`Entry::EvalUnary`] instead.
    nearest: bool,
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend").field("isa", &self.isa.name()).finish_non_exhaustive()
    }
}

impl Backend {
    /// The backend for this machine: its architecture, and on x86-64 the instruction set
    /// extensions the processor reports, since the code runs where it is compiled.
    ///
    /// # Errors
    ///
    /// On an architecture Cranelift is not built for here, or when a setting is refused.
    pub fn host() -> Result<Backend, Error> {
        let bad = |e: &dyn fmt::Display| fail("", e.to_string());
        let mut flags = settings::builder();
        let verify = if cfg!(debug_assertions) { "true" } else { "false" };
        for (name, value) in [
            ("opt_level", "speed"),
            ("regalloc_algorithm", "backtracking"),
            ("preserve_frame_pointers", "true"),
            ("enable_verifier", verify),
            ("is_pic", "false"),
            ("use_colocated_libcalls", "false"),
            ("unwind_info", "false"),
            ("enable_probestack", "false"),
        ] {
            flags.set(name, value).map_err(|e| bad(&e))?;
        }
        let triple = if cfg!(target_arch = "x86_64") {
            if cfg!(target_os = "macos") {
                "x86_64-apple-darwin"
            } else {
                "x86_64-unknown-linux-gnu"
            }
        } else if cfg!(target_arch = "aarch64") {
            if cfg!(target_os = "macos") {
                "aarch64-apple-darwin"
            } else {
                "aarch64-unknown-linux-gnu"
            }
        } else {
            return Err(fail("", "no Cranelift target for this architecture"));
        };
        let mut builder = isa::lookup_by_name(triple).map_err(|e| bad(&e))?;
        let nearest = host_flags(&mut builder)?;
        let isa = builder.finish(settings::Flags::new(flags)).map_err(|e| bad(&e))?;
        Ok(Backend { isa, nearest })
    }

    /// The target's name, for `EXPLAIN`.
    #[must_use]
    pub fn target(&self) -> String {
        format!("{}", self.isa.triple())
    }

    /// Compiles one function.
    ///
    /// # Errors
    ///
    /// When the function uses something this module does not lower, or Cranelift refuses it.
    pub fn compile(&self, f: &Func) -> Result<Function, Error> {
        let call = self.isa.default_call_conv();
        let mut sig = Signature::new(call);
        sig.params.push(AbiParam::new(types::I64));
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I64));
        let mut func = ir::Function::with_name_signature(UserFuncName::user(0, 0), sig);
        let mut fctx = FunctionBuilderContext::new();
        {
            let b = FunctionBuilder::new(&mut func, &mut fctx);
            Lower::new(f, b, call, self.nearest).run()?;
        }
        let mut ctx = Context::for_function(func);
        let mut plane = ControlPlane::default();
        let (bytes, found) = {
            let code = ctx
                .compile(&*self.isa, &mut plane)
                .map_err(|e| fail(&f.name, format!("{:?}", e.inner)))?;
            let found: Vec<_> = code
                .buffer
                .relocs()
                .iter()
                .map(|r| (r.offset, r.kind, r.target.clone(), r.addend))
                .collect();
            (code.code_buffer().to_vec(), found)
        };
        let names = ctx.func.params.user_named_funcs();
        let mut relocs = Vec::with_capacity(found.len());
        for (offset, kind, target, addend) in found {
            let entry = match (kind, &target) {
                (
                    binemit::Reloc::Abs8,
                    FinalizedRelocTarget::ExternalName(ExternalName::User(r)),
                ) => Entry::from_index(names[*r].index),
                _ => None,
            };
            let Some(entry) = entry else {
                return Err(fail(&f.name, format!("a relocation it cannot resolve: {kind:?}")));
            };
            relocs.push(Reloc { offset, entry, addend });
        }
        Ok(Function { name: f.name.clone(), bytes, relocs })
    }
}

/// Turns on the instruction set extensions this x86-64 processor has, which Cranelift assumes
/// absent unless told, and says whether `nearest` is an instruction.
#[cfg(target_arch = "x86_64")]
fn host_flags(builder: &mut isa::Builder) -> Result<bool, Error> {
    let has = [
        ("has_sse3", std::is_x86_feature_detected!("sse3")),
        ("has_ssse3", std::is_x86_feature_detected!("ssse3")),
        ("has_sse41", std::is_x86_feature_detected!("sse4.1")),
        ("has_sse42", std::is_x86_feature_detected!("sse4.2")),
        ("has_popcnt", std::is_x86_feature_detected!("popcnt")),
        ("has_avx", std::is_x86_feature_detected!("avx")),
        ("has_avx2", std::is_x86_feature_detected!("avx2")),
        ("has_bmi1", std::is_x86_feature_detected!("bmi1")),
        ("has_bmi2", std::is_x86_feature_detected!("bmi2")),
        ("has_lzcnt", std::is_x86_feature_detected!("lzcnt")),
    ];
    for (name, on) in has {
        builder
            .set(name, if on { "true" } else { "false" })
            .map_err(|e| fail("", e.to_string()))?;
    }
    Ok(std::is_x86_feature_detected!("sse4.1"))
}

/// AArch64 has every instruction this module uses in the base architecture.
#[cfg(not(target_arch = "x86_64"))]
fn host_flags(_builder: &mut isa::Builder) -> Result<bool, Error> {
    Ok(true)
}

/// The Cranelift type that holds a QIR type.
fn cty(ty: Ty) -> Option<Type> {
    Some(match ty {
        Ty::Void => return None,
        Ty::I1 | Ty::I8 => types::I8,
        Ty::I16 => types::I16,
        Ty::I32 => types::I32,
        Ty::I64 | Ty::Ptr => types::I64,
        Ty::I128 | Ty::Str16 => types::I128,
        Ty::F32 => types::F32,
        Ty::F64 => types::F64,
    })
}

/// The integer type of the same width, which is how a float is spilled and bitcast.
fn int_of(ty: Ty) -> Ty {
    match ty {
        Ty::F32 => Ty::I32,
        Ty::F64 => Ty::I64,
        other => other,
    }
}

fn wide(ty: Ty) -> bool {
    matches!(ty, Ty::I128 | Ty::Str16)
}

/// The blocks reachable from the entry, each after every block that dominates it.
fn reverse_postorder(f: &Func) -> Vec<Block> {
    let succs = |b: Block| {
        let mut out = Vec::new();
        if let Some(t) = f.terminator(b) {
            t.succs(|s, _| out.push(s));
        }
        out
    };
    let mut seen = vec![false; f.blocks.len()];
    let mut post = Vec::with_capacity(f.blocks.len());
    let mut stack = vec![(Block(0), succs(Block(0)), 0usize)];
    seen[0] = true;
    while let Some(top) = stack.last_mut() {
        if let Some(&next) = top.1.get(top.2) {
            top.2 += 1;
            if !seen[next.index()] {
                seen[next.index()] = true;
                stack.push((next, succs(next), 0));
            }
        } else {
            post.push(top.0);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// The state of lowering one function.
struct Lower<'a, 'b> {
    f: &'a Func,
    b: FunctionBuilder<'b>,
    call: CallConv,
    nearest: bool,
    /// The Cranelift value of each QIR value, once its definition has been lowered.
    vals: Vec<Option<Value>>,
    /// The Cranelift block of each reachable QIR block.
    blocks: Vec<Option<ir::Block>>,
    /// `%st`, from the entry block.
    st: Value,
    /// The runtime context word, loaded once in the entry block when anything calls the runtime.
    ctx: Option<Value>,
    /// Sixteen byte words for call arguments and for the operands of the `eval` entries, and one
    /// more for a call's result.
    slot: StackSlot,
    /// Where the out word of an `rtcall` is in `slot`.
    out: i32,
    /// The `poll` counter, when the function polls.
    polls: Option<StackSlot>,
    /// A cold block per status the function can return as a constant.
    exits: HashMap<u64, ir::Block>,
    /// A cold block that returns its one parameter, for a status a call returned.
    pass: Option<ir::Block>,
    funcs: HashMap<u32, FuncRef>,
    crc: Option<ir::GlobalValue>,
}

impl<'a, 'b> Lower<'a, 'b> {
    fn new(f: &'a Func, mut b: FunctionBuilder<'b>, call: CallConv, nearest: bool) -> Self {
        let mut words = 2usize;
        for b in 0..f.blocks.len() {
            for i in f.insts(Block(b as u32)) {
                match i.op.form() {
                    Form::Rtcall => words = words.max(i.ops.len() - 1),
                    Form::Vcall => words = words.max(i.ops.len() - 2),
                    _ => {}
                }
            }
        }
        let size = u32::try_from((words + 1) * 16).unwrap_or(u32::MAX);
        let slot =
            b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 4));
        let entry = b.create_block();
        b.append_block_params_for_function_params(entry);
        b.switch_to_block(entry);
        let st = b.block_params(entry)[0];
        Lower {
            f,
            b,
            call,
            nearest,
            vals: vec![None; f.vals.len()],
            blocks: vec![None; f.blocks.len()],
            st,
            ctx: None,
            slot,
            out: i32::try_from(words * 16).unwrap_or(i32::MAX),
            polls: None,
            exits: HashMap::new(),
            pass: None,
            funcs: HashMap::new(),
            crc: None,
        }
    }

    fn fail(&self, reason: impl Into<String>) -> Error {
        fail(&self.f.name, reason)
    }

    fn run(mut self) -> Result<(), Error> {
        let f = self.f;
        let order = reverse_postorder(f);
        let mut calls = false;
        let mut polls = false;
        for &blk in &order {
            for i in f.insts(blk).filter(|i| !i.dead()) {
                calls |= matches!(i.op.form(), Form::Rtcall | Form::Vcall | Form::CtrAdd);
                polls |= i.op == Op::Poll;
            }
            let block = self.b.create_block();
            for &p in &f.blocks[blk.index()].params {
                let ty = cty(f.ty(p)).ok_or_else(|| self.fail("a void block parameter"))?;
                let v = self.b.append_block_param(block, ty);
                self.vals[p.index()] = Some(v);
            }
            if f.blocks[blk.index()].cold {
                self.b.set_cold_block(block);
            }
            self.blocks[blk.index()] = Some(block);
        }
        if calls || polls {
            let word = self.b.ins().load(types::I64, MemFlags::trusted(), self.st, CTX_OFFSET);
            self.ctx = Some(word);
        }
        if polls {
            let slot = self.b.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                4,
                2,
            ));
            let zero = self.b.ins().iconst(types::I32, 0);
            self.b.ins().stack_store(zero, slot, 0);
            self.polls = Some(slot);
        }
        let entry = self.b.current_block().ok_or_else(|| self.fail("no entry block"))?;
        let params: Vec<BlockArg> =
            self.b.block_params(entry).iter().map(|v| BlockArg::Value(*v)).collect();
        let first = self.block(Block(0))?;
        if f.blocks[0].params.len() != 2 {
            return Err(self.fail("an entry block without (%st, %m)"));
        }
        self.b.ins().jump(first, &params);
        for &blk in &order {
            let block = self.block(blk)?;
            self.b.switch_to_block(block);
            for i in f.insts(blk).filter(|i| !i.dead()) {
                self.inst(&i)?;
            }
        }
        // The cold exits go last, in the order of their status so that the layout does not depend
        // on the order of a hash map.
        let mut exits: Vec<(u64, ir::Block)> = self.exits.iter().map(|(s, b)| (*s, *b)).collect();
        exits.sort_unstable_by_key(|(s, _)| *s);
        for (status, block) in exits {
            self.b.switch_to_block(block);
            self.b.set_cold_block(block);
            let v = self.b.ins().iconst(types::I64, status as i64);
            self.b.ins().return_(&[v]);
        }
        if let Some(block) = self.pass {
            self.b.switch_to_block(block);
            self.b.set_cold_block(block);
            let v = self.b.block_params(block)[0];
            self.b.ins().return_(&[v]);
        }
        self.b.seal_all_blocks();
        self.b.finalize();
        Ok(())
    }

    fn block(&self, b: Block) -> Result<ir::Block, Error> {
        match self.blocks.get(b.index()) {
            Some(Some(block)) => Ok(*block),
            _ => Err(self.fail(format!("a branch to block {} that is not reachable", b.0))),
        }
    }

    /// The Cranelift value of an operand word.
    fn val(&mut self, w: u32) -> Result<Value, Error> {
        let v = Val(w);
        if v.is_const() {
            let c = self
                .f
                .consts
                .get(v.const_index())
                .copied()
                .ok_or_else(|| self.fail("a constant past the pool"))?;
            return self.konst(c.ty, c.bits);
        }
        match self.vals.get(v.index()) {
            Some(Some(value)) => Ok(*value),
            _ => Err(self.fail(format!("v{} used before it is defined", v.0))),
        }
    }

    fn ty(&self, w: u32) -> Ty {
        self.f.ty(Val(w))
    }

    fn def(&mut self, i: &Inst<'_>, v: Value) {
        if let Some(r) = i.result {
            self.vals[r.index()] = Some(v);
        }
    }

    /// A constant of a QIR type.
    fn konst(&mut self, ty: Ty, bits: u128) -> Result<Value, Error> {
        let bits = bits & ty.mask();
        Ok(match ty {
            Ty::Void => return Err(self.fail("a void constant")),
            Ty::F32 => self.b.ins().f32const(Ieee32::with_bits(bits as u32)),
            Ty::F64 => self.b.ins().f64const(Ieee64::with_bits(bits as u64)),
            Ty::I128 | Ty::Str16 => {
                let lo = self.b.ins().iconst(types::I64, bits as u64 as i64);
                let hi = self.b.ins().iconst(types::I64, (bits >> 64) as u64 as i64);
                self.b.ins().iconcat(lo, hi)
            }
            _ => {
                let t = cty(ty).ok_or_else(|| self.fail("a void constant"))?;
                self.b.ins().iconst(t, bits as u64 as i64)
            }
        })
    }

    /// The low 64 bits of a value, zero extended, with a float as its bits.
    fn low64(&mut self, v: Value, ty: Ty) -> Value {
        match ty {
            Ty::F32 => {
                let i = self.b.ins().bitcast(types::I32, MemFlags::new(), v);
                self.b.ins().uextend(types::I64, i)
            }
            Ty::F64 => self.b.ins().bitcast(types::I64, MemFlags::new(), v),
            Ty::I128 | Ty::Str16 => self.b.ins().isplit(v).0,
            Ty::I64 | Ty::Ptr => v,
            _ => self.b.ins().uextend(types::I64, v),
        }
    }

    /// An integer value of type `from` as type `to`, extended the way `signed` says or cut.
    fn resize(&mut self, v: Value, from: Ty, to: Ty, signed: bool) -> Value {
        let (Some(cf), Some(ct)) = (cty(from), cty(to)) else { return v };
        // An `i1` is 0 or 1 in a byte, and sign extending it is negating it.
        let v = if signed && from == Ty::I1 { self.b.ins().ineg(v) } else { v };
        let v = if cf.bits() < ct.bits() {
            if signed { self.b.ins().sextend(ct, v) } else { self.b.ins().uextend(ct, v) }
        } else if cf.bits() > ct.bits() {
            let v = if cf == types::I128 { self.b.ins().isplit(v).0 } else { v };
            if ct == types::I64 { v } else { self.b.ins().ireduce(ct, v) }
        } else {
            v
        };
        if to == Ty::I1 && (from != Ty::I1 || signed) { self.b.ins().band_imm(v, 1) } else { v }
    }

    /// `bitcast`, `trunc` and `zext` as the interpreter does them: the bits, masked to `to`.
    fn bits(&mut self, v: Value, from: Ty, to: Ty) -> Value {
        if cty(from) == cty(to) {
            return if to == Ty::I1 && from != Ty::I1 { self.b.ins().band_imm(v, 1) } else { v };
        }
        let v = match from {
            Ty::F32 => self.b.ins().bitcast(types::I32, MemFlags::new(), v),
            Ty::F64 => self.b.ins().bitcast(types::I64, MemFlags::new(), v),
            _ => v,
        };
        let v = self.resize(v, int_of(from), int_of(to), false);
        match to {
            Ty::F32 => self.b.ins().bitcast(types::F32, MemFlags::new(), v),
            Ty::F64 => self.b.ins().bitcast(types::F64, MemFlags::new(), v),
            _ => v,
        }
    }

    /// The block that returns `status`.
    fn exit(&mut self, status: u64) -> ir::Block {
        if let Some(b) = self.exits.get(&status) {
            return *b;
        }
        let b = self.b.create_block();
        self.exits.insert(status, b);
        b
    }

    /// Returns `status` when `cond` is not zero, and carries on in a new block otherwise.
    fn exit_if(&mut self, cond: Value, status: u64) {
        let out = self.exit(status);
        let on = self.b.create_block();
        self.b.ins().brif(cond, out, &[], on, &[]);
        self.b.switch_to_block(on);
    }

    fn error_if(&mut self, cond: Value, site: u32) {
        self.exit_if(cond, status::make(status::ERROR, u64::from(site)));
    }

    /// Returns a status a call returned, unless it is zero.
    fn pass_if(&mut self, s: Value) {
        let pass = match self.pass {
            Some(b) => b,
            None => {
                let b = self.b.create_block();
                self.b.append_block_param(b, types::I64);
                self.pass = Some(b);
                b
            }
        };
        let on = self.b.create_block();
        self.b.ins().brif(s, pass, &[BlockArg::Value(s)], on, &[]);
        self.b.switch_to_block(on);
    }

    /// A reference to a runtime entry point.
    fn entry(&mut self, e: Entry) -> FuncRef {
        if let Some(f) = self.funcs.get(&e.index()) {
            return *f;
        }
        let mut sig = Signature::new(self.call);
        let (i32p, i64p) = (AbiParam::new(types::I32).uext(), AbiParam::new(types::I64));
        let (params, ret): (Vec<AbiParam>, Option<AbiParam>) = match e {
            Entry::Rtcall => (vec![i64p, i32p, i64p, i32p, i64p], Some(i64p)),
            Entry::Vcall => (vec![i64p, i32p, i64p, i64p, i32p], Some(i64p)),
            Entry::Count => (vec![i64p, i32p, i64p], None),
            Entry::Cancelled => (vec![i64p], Some(i32p)),
            Entry::EvalUnary | Entry::EvalScale => (vec![i32p, i32p, i32p, i64p], Some(i32p)),
            Entry::EvalBinary | Entry::Crc32cTable => (vec![i32p, i32p, i64p], Some(i32p)),
        };
        sig.params = params;
        sig.returns = ret.into_iter().collect();
        let signature = self.b.import_signature(sig);
        let name = self.b.func.declare_imported_user_function(UserExternalName::new(0, e.index()));
        let f = self.b.import_function(ExtFuncData {
            name: ExternalName::user(name),
            signature,
            colocated: false,
        });
        self.funcs.insert(e.index(), f);
        f
    }

    fn ctx(&self) -> Result<Value, Error> {
        self.ctx.ok_or_else(|| self.fail("a runtime call with no context loaded"))
    }

    fn u32c(&mut self, x: u32) -> Value {
        self.b.ins().iconst(types::I32, i64::from(x))
    }

    /// Writes a value to word `at` of the scratch slot as the interpreter holds it: its bits,
    /// zero extended to 128.
    fn spill(&mut self, v: Value, ty: Ty, at: usize) {
        let off = i32::try_from(at * 16).unwrap_or(i32::MAX);
        if wide(ty) {
            self.b.ins().stack_store(v, self.slot, off);
        } else {
            let lo = self.low64(v, ty);
            let zero = self.b.ins().iconst(types::I64, 0);
            self.b.ins().stack_store(lo, self.slot, off);
            self.b.ins().stack_store(zero, self.slot, off + 8);
        }
    }

    fn unspill(&mut self, ty: Ty, off: i32) -> Result<Value, Error> {
        let t = cty(ty).ok_or_else(|| self.fail("a void result"))?;
        Ok(self.b.ins().stack_load(t, self.slot, off))
    }

    /// One of the `eval` entries on `args`: the result as type `to`, and the flag that says it
    /// trapped.
    fn eval(
        &mut self,
        e: Entry,
        op: Op,
        ty: Ty,
        extra: u32,
        args: &[(Value, Ty)],
        to: Ty,
    ) -> Result<(Value, Value), Error> {
        for (at, (v, t)) in args.iter().enumerate() {
            self.spill(*v, *t, at);
        }
        let addr = self.b.ins().stack_addr(types::I64, self.slot, 0);
        let f = self.entry(e);
        let (o, t, x) = (self.u32c(op as u32), self.u32c(ty as u32), self.u32c(extra));
        let call = match e {
            Entry::EvalBinary => self.b.ins().call(f, &[o, t, addr]),
            _ => self.b.ins().call(f, &[o, t, x, addr]),
        };
        let trapped = self.b.inst_results(call)[0];
        let v = self.unspill(to, 0)?;
        Ok((v, trapped))
    }

    /// An `eval` entry whose trap returns error `site`.
    #[allow(clippy::too_many_arguments)]
    fn eval_or(
        &mut self,
        e: Entry,
        op: Op,
        ty: Ty,
        extra: u32,
        args: &[(Value, Ty)],
        to: Ty,
        site: Option<u32>,
    ) -> Result<Value, Error> {
        let (v, trapped) = self.eval(e, op, ty, extra, args, to)?;
        if let Some(site) = site {
            self.error_if(trapped, site);
        }
        Ok(v)
    }

    /// The signed minimum of `ty`.
    fn min(&mut self, ty: Ty) -> Result<Value, Error> {
        self.konst(ty, 1u128 << (ty.bits() - 1))
    }

    /// DuckDB's total order on floats: `a < b` with NaN above everything.
    fn total_lt(&mut self, a: Value, b: Value) -> Value {
        let lt = self.b.ins().fcmp(FloatCC::LessThan, a, b);
        let anan = self.b.ins().fcmp(FloatCC::Unordered, a, a);
        let bnan = self.b.ins().fcmp(FloatCC::Unordered, b, b);
        let anum = self.b.ins().bxor_imm(anan, 1);
        let nan = self.b.ins().band(anum, bnan);
        self.b.ins().bor(lt, nan)
    }

    /// The total order's equality: NaN equals NaN, and `-0` equals `0`.
    fn total_eq(&mut self, a: Value, b: Value) -> Value {
        let eq = self.b.ins().fcmp(FloatCC::Equal, a, b);
        let anan = self.b.ins().fcmp(FloatCC::Unordered, a, a);
        let bnan = self.b.ins().fcmp(FloatCC::Unordered, b, b);
        let both = self.b.ins().band(anan, bnan);
        self.b.ins().bor(eq, both)
    }

    /// Checked add and subtract of two 128 bit values: the result and whether it overflowed.
    fn checked128(&mut self, op: Op, a: Value, b: Value) -> (Value, Value) {
        let (r, of) = match op {
            Op::SaddT | Op::SaddOv => {
                let r = self.b.ins().iadd(a, b);
                let x = self.b.ins().bxor(a, r);
                let y = self.b.ins().bxor(b, r);
                (r, self.b.ins().band(x, y))
            }
            Op::SsubT | Op::SsubOv => {
                let r = self.b.ins().isub(a, b);
                let x = self.b.ins().bxor(a, b);
                let y = self.b.ins().bxor(a, r);
                (r, self.b.ins().band(x, y))
            }
            Op::UaddT => {
                let r = self.b.ins().iadd(a, b);
                return (r, self.b.ins().icmp(IntCC::UnsignedLessThan, r, a));
            }
            _ => {
                let r = self.b.ins().isub(a, b);
                return (r, self.b.ins().icmp(IntCC::UnsignedLessThan, a, b));
            }
        };
        let hi = self.b.ins().isplit(of).1;
        (r, self.b.ins().icmp_imm(IntCC::SignedLessThan, hi, 0))
    }

    /// Checked arithmetic up to 64 bits: the result and whether it overflowed.
    fn checked(&mut self, op: Op, a: Value, b: Value) -> (Value, Value) {
        match op {
            Op::SaddT | Op::SaddOv => self.b.ins().sadd_overflow(a, b),
            Op::SsubT | Op::SsubOv => self.b.ins().ssub_overflow(a, b),
            Op::SmulT | Op::SmulOv => self.b.ins().smul_overflow(a, b),
            Op::UaddT => self.b.ins().uadd_overflow(a, b),
            Op::UsubT => self.b.ins().usub_overflow(a, b),
            _ => self.b.ins().umul_overflow(a, b),
        }
    }

    fn inst(&mut self, i: &Inst<'_>) -> Result<(), Error> {
        let o = i.ops;
        let ty = i.ty;
        match i.op.form() {
            Form::Un | Form::Conv => {
                let from = self.ty(o[0]);
                let to = i.op.result(ty);
                let a = self.val(o[0])?;
                let r = self.unary(i.op, from, to, a)?;
                self.def(i, r);
            }
            Form::Bin | Form::Cmp | Form::Wide => {
                let (a, b) = (self.val(o[0])?, self.val(o[1])?);
                let r = self.binary(i.op, ty, a, b)?;
                self.def(i, r);
            }
            Form::StrMk => {
                let (a, b) = (self.val(o[0])?, self.val(o[1])?);
                let (lo, hi) = (self.low64(a, self.ty(o[0])), self.low64(b, self.ty(o[1])));
                let r = self.b.ins().iconcat(lo, hi);
                self.def(i, r);
            }
            Form::Sel => {
                let (c, a, b) = (self.val(o[0])?, self.val(o[1])?, self.val(o[2])?);
                let r = if matches!(ty, Ty::I128 | Ty::Str16) {
                    // The aarch64 backend of Cranelift 0.125 panics on a select of 128 bits whose
                    // condition is a compare of 128 bits, so each half is selected on its own.
                    let ((a_lo, a_hi), (b_lo, b_hi)) =
                        (self.b.ins().isplit(a), self.b.ins().isplit(b));
                    let lo = self.b.ins().select(c, a_lo, b_lo);
                    let hi = self.b.ins().select(c, a_hi, b_hi);
                    self.b.ins().iconcat(lo, hi)
                } else {
                    self.b.ins().select(c, a, b)
                };
                self.def(i, r);
            }
            Form::TrapBin => {
                let (a, b) = (self.val(o[0])?, self.val(o[1])?);
                let r = self.trap_binary(i.op, ty, a, b, o[2])?;
                self.def(i, r);
            }
            Form::TrapUn => {
                let a = self.val(o[0])?;
                let r = if ty == Ty::I1 || ty.is_float() {
                    self.eval_or(Entry::EvalUnary, i.op, ty, ty as u32, &[(a, ty)], ty, Some(o[1]))?
                } else {
                    let min = self.min(ty)?;
                    let bad = self.b.ins().icmp(IntCC::Equal, a, min);
                    self.error_if(bad, o[1]);
                    self.b.ins().ineg(a)
                };
                self.def(i, r);
            }
            Form::TrapConv => {
                let from = self.ty(o[0]);
                let a = self.val(o[0])?;
                let r = self.ftosi(from, ty, a, o[1])?;
                self.def(i, r);
            }
            Form::Scale | Form::TrapScale => {
                let a = self.val(o[0])?;
                let site = (i.op.form() == Form::TrapScale).then(|| o[2]);
                let r = self.scale(i.op, ty, a, o[1], site)?;
                self.def(i, r);
            }
            Form::EdgeBin => {
                let (a, b) = (self.val(o[0])?, self.val(o[1])?);
                let (r, of) = if ty == Ty::I1 || (wide(ty) && i.op == Op::SmulOv) {
                    self.eval(Entry::EvalBinary, i.op, ty, 0, &[(a, ty), (b, ty)], ty)?
                } else if wide(ty) {
                    self.checked128(i.op, a, b)
                } else {
                    self.checked(i.op, a, b)
                };
                let (ok, ovf) = (self.block(Block(o[2]))?, self.block(Block(o[3]))?);
                self.b.ins().brif(of, ovf, &[], ok, &[BlockArg::Value(r)]);
            }
            Form::Load => {
                let (addr, disp) = self.address(o)?;
                let t = cty(ty).ok_or_else(|| self.fail("a void load"))?;
                let v = self.b.ins().load(t, MEM, addr, disp);
                let v = if ty == Ty::I1 { self.b.ins().band_imm(v, 1) } else { v };
                let v = self.bits(v, ty, i.op.result(ty));
                self.def(i, v);
            }
            Form::Store => {
                let (addr, disp) = self.address(o)?;
                let v = self.val(o[4])?;
                let v = self.bits(v, self.ty(o[4]), ty);
                self.b.ins().store(MEM, v, addr, disp);
            }
            Form::LoadBit => {
                let base = self.val(o[0])?;
                let idx = self.val(o[1])?;
                let idx = self.low64(idx, self.ty(o[1]));
                let byte = self.b.ins().ushr_imm(idx, 3);
                let at = self.b.ins().iadd(base, byte);
                let v = self.b.ins().load(types::I8, MEM, at, 0);
                let bit = self.b.ins().band_imm(idx, 7);
                let v = self.b.ins().ushr(v, bit);
                let v = self.b.ins().band_imm(v, 1);
                self.def(i, v);
            }
            Form::Memcpy => {
                let (dst, src) = (self.val(o[0])?, self.val(o[1])?);
                let flags = MEM;
                // Every chunk is read before any is written, which is what makes an overlap copy
                // the way `ptr::copy` does.
                let mut chunks = Vec::new();
                for (off, t) in pieces(o[2]) {
                    chunks.push((off, self.b.ins().load(t, flags, src, off)));
                }
                for (off, v) in chunks {
                    self.b.ins().store(flags, v, dst, off);
                }
            }
            Form::Memeq => {
                let (a, b) = (self.val(o[0])?, self.val(o[1])?);
                let flags = MEM;
                let mut diff = self.b.ins().iconst(types::I64, 0);
                for (off, t) in pieces(o[2]) {
                    let x = self.b.ins().load(t, flags, a, off);
                    let y = self.b.ins().load(t, flags, b, off);
                    let d = self.b.ins().bxor(x, y);
                    let d = if t == types::I64 { d } else { self.b.ins().uextend(types::I64, d) };
                    diff = self.b.ins().bor(diff, d);
                }
                let r = self.b.ins().icmp_imm(IntCC::Equal, diff, 0);
                self.def(i, r);
            }
            Form::Prefetch => {}
            Form::Cas => {
                // One thread per state, as in the interpreter: a compare and a write.
                let (at, old, new) = (self.val(o[0])?, self.val(o[1])?, self.val(o[2])?);
                let t = int_of(ty);
                let ct = cty(t).ok_or_else(|| self.fail("a void cas"))?;
                let flags = MEM;
                // A miss writes back the bytes it read, which for an `i1` may have more than the
                // low bit set, so that the memory is what the interpreter would leave.
                let raw = self.b.ins().load(ct, flags, at, 0);
                let cur = if ty == Ty::I1 { self.b.ins().band_imm(raw, 1) } else { raw };
                let old = self.bits(old, self.ty(o[1]), t);
                let new = self.bits(new, self.ty(o[2]), t);
                let hit = self.b.ins().icmp(IntCC::Equal, cur, old);
                let v = self.b.ins().select(hit, new, raw);
                self.b.ins().store(flags, v, at, 0);
                self.def(i, hit);
            }
            Form::Atomic => {
                let (at, v) = (self.val(o[0])?, self.val(o[1])?);
                if ty.is_float() || ty == Ty::I1 {
                    return Err(self.fail(format!("atomic.add of {}", ty.name())));
                }
                let ct = cty(ty).ok_or_else(|| self.fail("a void atomic"))?;
                let flags = MEM;
                let cur = self.b.ins().load(ct, flags, at, 0);
                let v = self.bits(v, self.ty(o[1]), ty);
                let sum = self.b.ins().iadd(cur, v);
                self.b.ins().store(flags, sum, at, 0);
                self.def(i, cur);
            }
            Form::Br => {
                let target = self.block(Block(o[0]))?;
                let args = self.args(&o[1..])?;
                self.b.ins().jump(target, &args);
            }
            Form::Brif => {
                let c = self.val(o[0])?;
                let n = o[2] as usize;
                let t = self.block(Block(o[1]))?;
                let targs = self.args(&o[3..3 + n])?;
                let e = self.block(Block(o[3 + n]))?;
                let eargs = self.args(&o[4 + n..])?;
                self.b.ins().brif(c, t, &targs, e, &eargs);
            }
            Form::Switch => self.switch(o)?,
            Form::Ret => {
                let v = self.val(o[0])?;
                let v = self.low64(v, self.ty(o[0]));
                self.b.ins().return_(&[v]);
            }
            Form::Trap => {
                let v = self
                    .b
                    .ins()
                    .iconst(types::I64, status::make(status::ERROR, u64::from(o[0])) as i64);
                self.b.ins().return_(&[v]);
            }
            Form::Rtcall => {
                let args: Vec<u32> = o[1..].to_vec();
                for (at, w) in args.iter().enumerate() {
                    let v = self.val(*w)?;
                    self.spill(v, self.ty(*w), at);
                }
                let ctx = self.ctx()?;
                let f = self.entry(Entry::Rtcall);
                let proxy = self.u32c(o[0]);
                let addr = self.b.ins().stack_addr(types::I64, self.slot, 0);
                let n = self.u32c(u32::try_from(args.len()).unwrap_or(u32::MAX));
                let out = self.b.ins().stack_addr(types::I64, self.slot, self.out);
                let call = self.b.ins().call(f, &[ctx, proxy, addr, n, out]);
                let s = self.b.inst_results(call)[0];
                self.pass_if(s);
                if i.result.is_some() {
                    let t = i.op.result(ty);
                    let v = self.unspill(t, self.out)?;
                    let v = if t == Ty::I1 { self.b.ins().band_imm(v, 1) } else { v };
                    self.def(i, v);
                }
            }
            Form::Vcall => {
                let bufs: Vec<u32> = o[2..].to_vec();
                for (at, w) in bufs.iter().enumerate() {
                    let v = self.val(*w)?;
                    self.spill(v, self.ty(*w), at);
                }
                let ctx = self.ctx()?;
                let f = self.entry(Entry::Vcall);
                let kernel = self.u32c(o[0]);
                let n = self.val(o[1])?;
                let n = self.low64(n, self.ty(o[1]));
                let addr = self.b.ins().stack_addr(types::I64, self.slot, 0);
                let count = self.u32c(u32::try_from(bufs.len()).unwrap_or(u32::MAX));
                let call = self.b.ins().call(f, &[ctx, kernel, n, addr, count]);
                let s = self.b.inst_results(call)[0];
                self.pass_if(s);
            }
            Form::Guard => {
                let c = self.val(o[0])?;
                let out = self.exit(status::make(status::DEOPT, u64::from(o[1])));
                let on = self.b.create_block();
                self.b.ins().brif(c, on, &[], out, &[]);
                self.b.switch_to_block(on);
            }
            Form::Poll => self.poll(o[0].max(1))?,
            Form::CtrAdd => {
                let v = self.val(o[1])?;
                let v = self.low64(v, self.ty(o[1]));
                let ctx = self.ctx()?;
                let f = self.entry(Entry::Count);
                let k = self.u32c(o[0]);
                self.b.ins().call(f, &[ctx, k, v]);
            }
        }
        Ok(())
    }

    fn args(&mut self, words: &[u32]) -> Result<Vec<BlockArg>, Error> {
        words.iter().map(|w| self.val(*w).map(BlockArg::Value)).collect()
    }

    /// The address and displacement of a load or a store: `base + idx * scale`, and `disp`.
    fn address(&mut self, o: &[u32]) -> Result<(Value, i32), Error> {
        let mut addr = self.val(o[0])?;
        if o[1] != Val::NONE.0 {
            let idx = self.val(o[1])?;
            let idx = self.low64(idx, self.ty(o[1]));
            let scaled = if o[2] == 1 { idx } else { self.b.ins().imul_imm(idx, i64::from(o[2])) };
            addr = self.b.ins().iadd(addr, scaled);
        }
        Ok((addr, o[3] as i32))
    }

    fn switch(&mut self, o: &[u32]) -> Result<(), Error> {
        let x = self.val(o[0])?;
        let xty = self.ty(o[0]);
        let n = o[2] as usize;
        let default = self.block(Block(o[1]))?;
        let dargs = self.args(&o[3..3 + n])?;
        // A default with arguments needs a block of its own to pass them, since a switch's
        // fallthrough edge carries none.
        let otherwise = if dargs.is_empty() { default } else { self.b.create_block() };
        let mut sw = Switch::new();
        let mut seen = std::collections::HashSet::new();
        for pair in o[3 + n..].as_chunks::<2>().0 {
            let key = u128::from(pair[0]);
            // The interpreter takes the first case with a key, and a key wider than the value
            // never matches.
            if key & xty.mask() != key || !seen.insert(key) {
                continue;
            }
            sw.set_entry(key, self.block(Block(pair[1]))?);
        }
        let x = if xty.is_float() { self.bits(x, xty, int_of(xty)) } else { x };
        sw.emit(&mut self.b, x, otherwise);
        if otherwise != default {
            self.b.switch_to_block(otherwise);
            self.b.ins().jump(default, &dargs);
        }
        Ok(())
    }

    fn poll(&mut self, n: u32) -> Result<(), Error> {
        let slot = self.polls.ok_or_else(|| self.fail("a poll with no counter"))?;
        let c = self.b.ins().stack_load(types::I32, slot, 0);
        let c = self.b.ins().iadd_imm(c, 1);
        let hit = self.b.ins().icmp_imm(IntCC::UnsignedGreaterThanOrEqual, c, i64::from(n));
        let zero = self.b.ins().iconst(types::I32, 0);
        let next = self.b.ins().select(hit, zero, c);
        self.b.ins().stack_store(next, slot, 0);
        let ask = self.b.create_block();
        let on = self.b.create_block();
        self.b.ins().brif(hit, ask, &[], on, &[]);
        self.b.switch_to_block(ask);
        self.b.set_cold_block(ask);
        let ctx = self.ctx()?;
        let f = self.entry(Entry::Cancelled);
        let call = self.b.ins().call(f, &[ctx]);
        let yes = self.b.inst_results(call)[0];
        let out = self.exit(status::make(status::CANCELLED, 0));
        self.b.ins().brif(yes, out, &[], on, &[]);
        self.b.switch_to_block(on);
        Ok(())
    }

    fn unary(&mut self, op: Op, from: Ty, to: Ty, a: Value) -> Result<Value, Error> {
        let one = from == Ty::I1;
        // The float opcodes take floats and the rest take integers. Anything else is left to the
        // helper, which computes whatever the interpreter computes for it.
        let float_op = matches!(op, Op::Fneg | Op::Fabs | Op::Fsqrt | Op::Fext | Op::Ftrunc);
        let mixed = matches!(op, Op::Bitcast | Op::Sitof | Op::Uitof);
        if !mixed && (float_op != from.is_float() || (op != Op::Fsqrt && float_op != to.is_float()))
        {
            return self.eval_or(Entry::EvalUnary, op, from, to as u32, &[(a, from)], to, None);
        }
        Ok(match op {
            Op::Neg if one => a,
            Op::Neg => self.b.ins().ineg(a),
            Op::Not if one => self.b.ins().bxor_imm(a, 1),
            Op::Not => self.b.ins().bnot(a),
            Op::Clz | Op::Ctz if one => self.b.ins().bxor_imm(a, 1),
            Op::Clz => self.b.ins().clz(a),
            Op::Ctz => self.b.ins().ctz(a),
            Op::Popcnt if one => a,
            Op::Popcnt => self.b.ins().popcnt(a),
            Op::Bswap if one => self.b.ins().iconst(types::I8, 0),
            Op::Bswap if from == Ty::I8 => a,
            Op::Bswap => self.b.ins().bswap(a),
            Op::Sext => self.resize(a, from, to, true),
            Op::Zext | Op::Trunc => self.resize(a, from, to, false),
            Op::Bitcast => self.bits(a, from, to),
            Op::Sitof | Op::Uitof if !wide(from) && !from.is_float() && to.is_float() => {
                // The interpreter converts to `f64` and then to `f32`, and so does this, because
                // rounding twice is not always rounding once.
                let v = self.resize(a, from, Ty::I64, op == Op::Sitof);
                let d = if op == Op::Sitof {
                    self.b.ins().fcvt_from_sint(types::F64, v)
                } else {
                    self.b.ins().fcvt_from_uint(types::F64, v)
                };
                if to == Ty::F32 { self.b.ins().fdemote(types::F32, d) } else { d }
            }
            Op::Fext if from == Ty::F32 && to.is_float() => {
                let d = self.b.ins().fpromote(types::F64, a);
                if to == Ty::F32 { self.b.ins().fdemote(types::F32, d) } else { d }
            }
            Op::Ftrunc if from == Ty::F64 && to.is_float() => {
                if to == Ty::F32 {
                    self.b.ins().fdemote(types::F32, a)
                } else {
                    a
                }
            }
            // Cranelift's `fneg` and `fabs` change only the sign bit, as the interpreter does, so a NaN keeps its bits.
            Op::Fneg if from.is_float() => self.b.ins().fneg(a),
            Op::Fabs if from.is_float() => self.b.ins().fabs(a),
            Op::Fsqrt if from.is_float() && to == from => self.b.ins().sqrt(a),
            Op::StrLen => {
                let lo = self.low64(a, from);
                self.b.ins().ireduce(types::I32, lo)
            }
            Op::StrW0 => self.low64(a, from),
            Op::StrW1 | Op::StrPtr if wide(from) => self.b.ins().isplit(a).1,
            Op::StrInl => {
                let lo = self.low64(a, from);
                let len = self.b.ins().ireduce(types::I32, lo);
                self.b.ins().icmp_imm(IntCC::UnsignedLessThanOrEqual, len, 12)
            }
            _ => self.eval_or(Entry::EvalUnary, op, from, to as u32, &[(a, from)], to, None)?,
        })
    }

    fn binary(&mut self, op: Op, ty: Ty, a: Value, b: Value) -> Result<Value, Error> {
        let one = ty == Ty::I1;
        let fallback = |this: &mut Self| {
            let to = op.result(ty);
            this.eval_or(Entry::EvalBinary, op, ty, 0, &[(a, ty), (b, ty)], to, None)
        };
        let signed = |cc: IntCC| {
            matches!(
                cc,
                IntCC::SignedLessThan
                    | IntCC::SignedLessThanOrEqual
                    | IntCC::SignedGreaterThan
                    | IntCC::SignedGreaterThanOrEqual
            )
        };
        let float_op = matches!(
            op,
            Op::Fadd
                | Op::Fsub
                | Op::Fmul
                | Op::Fdiv
                | Op::FminTot
                | Op::FmaxTot
                | Op::FcmpEq
                | Op::FcmpLt
                | Op::FcmpLe
        );
        if float_op != ty.is_float() || (op == Op::Crc32c && (ty.bits() < 32 || ty.is_float())) {
            return fallback(self);
        }
        Ok(match op {
            Op::Add | Op::Sub if one => self.b.ins().bxor(a, b),
            Op::Mul if one => self.b.ins().band(a, b),
            Op::Add => self.b.ins().iadd(a, b),
            Op::Sub => self.b.ins().isub(a, b),
            Op::Mul => self.b.ins().imul(a, b),
            Op::And => self.b.ins().band(a, b),
            Op::Or => self.b.ins().bor(a, b),
            Op::Xor => self.b.ins().bxor(a, b),
            // An amount is taken modulo the width, and every amount is 0 modulo 1.
            Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr if one => a,
            Op::Shl | Op::Lshr | Op::Ashr | Op::Rotl | Op::Rotr => {
                let amount = if wide(ty) { self.b.ins().isplit(b).0 } else { b };
                match op {
                    Op::Shl => self.b.ins().ishl(a, amount),
                    Op::Lshr => self.b.ins().ushr(a, amount),
                    Op::Ashr => self.b.ins().sshr(a, amount),
                    Op::Rotl => self.b.ins().rotl(a, amount),
                    _ => self.b.ins().rotr(a, amount),
                }
            }
            Op::Smulw | Op::Umulw => {
                let s = op == Op::Smulw;
                let (x, y) = (self.resize(a, ty, Ty::I128, s), self.resize(b, ty, Ty::I128, s));
                self.b.ins().imul(x, y)
            }
            Op::Fadd => self.b.ins().fadd(a, b),
            Op::Fsub => self.b.ins().fsub(a, b),
            Op::Fmul => self.b.ins().fmul(a, b),
            Op::Fdiv => self.b.ins().fdiv(a, b),
            Op::FminTot | Op::FmaxTot => {
                let lt = self.total_lt(a, b);
                if op == Op::FminTot {
                    self.b.ins().select(lt, a, b)
                } else {
                    self.b.ins().select(lt, b, a)
                }
            }
            Op::FcmpEq => self.total_eq(a, b),
            Op::FcmpLt => self.total_lt(a, b),
            Op::FcmpLe => {
                let lt = self.total_lt(a, b);
                let eq = self.total_eq(a, b);
                self.b.ins().bor(lt, eq)
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
                if !ty.is_float() =>
            {
                let cc = match op {
                    Op::IcmpEq => IntCC::Equal,
                    Op::IcmpNe => IntCC::NotEqual,
                    Op::IcmpSlt => IntCC::SignedLessThan,
                    Op::IcmpSle => IntCC::SignedLessThanOrEqual,
                    Op::IcmpSgt => IntCC::SignedGreaterThan,
                    Op::IcmpSge => IntCC::SignedGreaterThanOrEqual,
                    Op::IcmpUlt => IntCC::UnsignedLessThan,
                    Op::IcmpUle => IntCC::UnsignedLessThanOrEqual,
                    Op::IcmpUgt => IntCC::UnsignedGreaterThan,
                    _ => IntCC::UnsignedGreaterThanOrEqual,
                };
                // A signed `i1` is 0 or -1.
                let (a, b) = if one && signed(cc) {
                    (self.b.ins().ineg(a), self.b.ins().ineg(b))
                } else {
                    (a, b)
                };
                self.b.ins().icmp(cc, a, b)
            }
            Op::Crc32c => self.crc32c(a, b, ty),
            _ => fallback(self)?,
        })
    }

    /// One step of CRC-32C over a 64 bit word, slice by eight.
    fn crc32c(&mut self, a: Value, b: Value, ty: Ty) -> Value {
        let seed = self.low64(a, ty);
        let word = self.low64(b, ty);
        let gv = match self.crc {
            Some(gv) => gv,
            None => {
                let name = self.b.func.declare_imported_user_function(UserExternalName::new(
                    0,
                    Entry::Crc32cTable.index(),
                ));
                let gv = self.b.create_global_value(GlobalValueData::Symbol {
                    name: ExternalName::user(name),
                    offset: Imm64::new(0),
                    colocated: false,
                    tls: false,
                });
                self.crc = Some(gv);
                gv
            }
        };
        let tables = self.b.ins().symbol_value(types::I64, gv);
        let flags = MemFlags::trusted().with_readonly();
        let one = self.b.ins().bxor(word, seed);
        let one = self.b.ins().band_imm(one, 0xffff_ffff);
        let two = self.b.ins().ushr_imm(word, 32);
        let mut crc: Option<Value> = None;
        // Byte `k` of the word goes through table `7 - k`, per `crate::ir::entry::crc32c_sliced`.
        for k in 0..8u32 {
            let (half, shift) = if k < 4 { (one, 8 * k) } else { (two, 8 * (k - 4)) };
            let byte =
                if shift == 0 { half } else { self.b.ins().ushr_imm(half, i64::from(shift)) };
            let byte = self.b.ins().band_imm(byte, 0xff);
            let off = self.b.ins().ishl_imm(byte, 2);
            let at = self.b.ins().iadd(tables, off);
            let t = i32::try_from((7 - k) * 1024).unwrap_or(0);
            let v = self.b.ins().load(types::I32, flags, at, t);
            crc = Some(match crc {
                Some(c) => self.b.ins().bxor(c, v),
                None => v,
            });
        }
        let crc = crc.unwrap_or(seed);
        self.b.ins().uextend(types::I64, crc)
    }

    fn trap_binary(
        &mut self,
        op: Op,
        ty: Ty,
        a: Value,
        b: Value,
        site: u32,
    ) -> Result<Value, Error> {
        let helper = |this: &mut Self| {
            this.eval_or(Entry::EvalBinary, op, ty, 0, &[(a, ty), (b, ty)], ty, Some(site))
        };
        if ty == Ty::I1 || ty.is_float() {
            return helper(self);
        }
        Ok(match op {
            Op::SaddT | Op::SsubT | Op::UaddT | Op::UsubT if wide(ty) => {
                let (r, of) = self.checked128(op, a, b);
                self.error_if(of, site);
                r
            }
            Op::SmulT | Op::UmulT if wide(ty) => helper(self)?,
            Op::SaddT | Op::SsubT | Op::SmulT | Op::UaddT | Op::UsubT | Op::UmulT => {
                let (r, of) = self.checked(op, a, b);
                self.error_if(of, site);
                r
            }
            Op::SdivT | Op::SremT | Op::UdivT | Op::UremT if wide(ty) => helper(self)?,
            Op::SdivT | Op::SremT | Op::UdivT | Op::UremT => {
                let zero = self.b.ins().icmp_imm(IntCC::Equal, b, 0);
                self.error_if(zero, site);
                match op {
                    Op::SdivT => {
                        let minus = self.konst(ty, u128::MAX)?;
                        let min = self.min(ty)?;
                        let x = self.b.ins().icmp(IntCC::Equal, b, minus);
                        let y = self.b.ins().icmp(IntCC::Equal, a, min);
                        let both = self.b.ins().band(x, y);
                        self.error_if(both, site);
                        self.b.ins().sdiv(a, b)
                    }
                    // `x % -1` is 0 for every `x`, `MIN` included, and Cranelift's `srem` says so
                    // without trapping.
                    Op::SremT => self.b.ins().srem(a, b),
                    Op::UdivT => self.b.ins().udiv(a, b),
                    _ => self.b.ins().urem(a, b),
                }
            }
            _ => helper(self)?,
        })
    }

    /// `ftosi.t`: round half to even, trap outside the target's range or on NaN.
    fn ftosi(&mut self, from: Ty, to: Ty, a: Value, site: u32) -> Result<Value, Error> {
        if !self.nearest || !from.is_float() || to.is_float() || wide(to) || to == Ty::I1 {
            return self.eval_or(
                Entry::EvalUnary,
                Op::FtosiT,
                from,
                to as u32,
                &[(a, from)],
                to,
                Some(site),
            );
        }
        let x = if from == Ty::F32 { self.b.ins().fpromote(types::F64, a) } else { a };
        let x = self.b.ins().nearest(x);
        let lo = -(2f64.powi(to.bits() as i32 - 1));
        let lo_v = self.b.ins().f64const(lo);
        let hi_v = self.b.ins().f64const(-lo);
        let nan = self.b.ins().fcmp(FloatCC::Unordered, x, x);
        let under = self.b.ins().fcmp(FloatCC::LessThan, x, lo_v);
        let over = self.b.ins().fcmp(FloatCC::GreaterThanOrEqual, x, hi_v);
        let bad = self.b.ins().bor(nan, under);
        let bad = self.b.ins().bor(bad, over);
        self.error_if(bad, site);
        let v = self.b.ins().fcvt_to_sint_sat(types::I64, x);
        Ok(self.resize(v, Ty::I64, to, false))
    }

    /// `dup.t` and `ddown` by `10^k`.
    fn scale(
        &mut self,
        op: Op,
        ty: Ty,
        a: Value,
        k: u32,
        site: Option<u32>,
    ) -> Result<Value, Error> {
        if ty == Ty::I1 || wide(ty) || ty.is_float() || !matches!(op, Op::DupT | Op::Ddown) {
            return self.eval_or(Entry::EvalScale, op, ty, k, &[(a, ty)], ty, site);
        }
        let p = crate::ir::eval::pow10(k.min(38));
        let max = (ty.mask() >> 1) as i128;
        let t = cty(ty).ok_or_else(|| self.fail("a void rescale"))?;
        if op == Op::DupT {
            let site = site.ok_or_else(|| self.fail("dup.t with no error site"))?;
            if p > max {
                // Only 0 times a power of ten this large fits.
                let nonzero = self.b.ins().icmp_imm(IntCC::NotEqual, a, 0);
                self.error_if(nonzero, site);
                return Ok(a);
            }
            let pv = self.konst(ty, p as u128)?;
            let (r, of) = self.b.ins().smul_overflow(a, pv);
            self.error_if(of, site);
            return Ok(r);
        }
        if p == 1 {
            return Ok(a);
        }
        // Round half away from zero: add the sign of `x` when twice the remainder reaches `p`.
        let neg = self.b.ins().icmp_imm(IntCC::SignedLessThan, a, 0);
        let minus = self.konst(ty, u128::MAX)?;
        let plus = self.b.ins().iconst(t, 1);
        let sign = self.b.ins().select(neg, minus, plus);
        let zero = self.b.ins().iconst(t, 0);
        if p > max {
            // The quotient is 0 and the remainder is `x`.
            let half = p / 2;
            if half as u128 > ty.mask() {
                return Ok(zero);
            }
            let ax = self.b.ins().iabs(a);
            let halfv = self.konst(ty, half as u128)?;
            let up = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, ax, halfv);
            return Ok(self.b.ins().select(up, sign, zero));
        }
        let pv = self.konst(ty, p as u128)?;
        let q = self.b.ins().sdiv(a, pv);
        let r = self.b.ins().srem(a, pv);
        let ar = self.b.ins().iabs(r);
        let twice = self.b.ins().ishl_imm(ar, 1);
        let up = self.b.ins().icmp(IntCC::UnsignedGreaterThanOrEqual, twice, pv);
        let add = self.b.ins().select(up, sign, zero);
        Ok(self.b.ins().iadd(q, add))
    }
}

/// The loads that cover `n` bytes, widest first: offsets and types.
fn pieces(n: u32) -> Vec<(i32, Type)> {
    let mut out = Vec::new();
    let mut at = 0u32;
    for (size, t) in [(8u32, types::I64), (4, types::I32), (2, types::I16), (1, types::I8)] {
        while n - at >= size {
            out.push((at as i32, t));
            at += size;
        }
    }
    out
}

#[cfg(test)]
mod tests;
